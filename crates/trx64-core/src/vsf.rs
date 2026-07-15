//! vsf.rs — VICE Snapshot Format (VSF) save/load for Machine state.
//!
//! Two formats are handled (ADR-075):
//!
//! 1. **c64re-own VSF** (save + load) — the compact framing c64re writes
//!    (null-terminated names, size EXCLUDES the header). `save_vsf` emits it;
//!    `load_vsf` parses it. Byte-for-byte parity with c64re is enforced by the
//!    parity probe (tests/vsf_parity_probe.rs) against a golden c64re-produced
//!    reset VSF — every machine-state module matches exactly.
//!    Reference:
//!      C64ReverseEngineeringMCP/src/runtime/headless/vsf/vsf-format.ts
//!      C64ReverseEngineeringMCP/src/runtime/headless/vsf/module-mapping.ts
//!      C64ReverseEngineeringMCP/src/runtime/headless/vsf/session-vsf.ts
//!
//! 2. **real VICE x64sc VSF** (load only) — a genuine VICE 3.7+ snapshot uses a
//!    58-byte header + 16-byte-padded module names + a size dword that INCLUDES
//!    the 22-byte module header. Detected by the "SIDEXTENDED" module name
//!    (c64re never writes it) and parsed by `load_vice_vsf` — MAINCPU/C64MEM/
//!    CIA1/CIA2/SID + the VIC-II head (model + 64 regs). The 123 KB VIC-IISC
//!    pipeline blob + the drive modules are skipped (cannot reconstruct / out of
//!    scope); enough is mapped to RESUME to a sane visible state. WRITING real
//!    VICE VSF is deferred (needs the off-limits viciisc pipeline blob).
//!    Reference:
//!      C64ReverseEngineeringMCP/src/runtime/headless/vsf/vice-vsf-load.ts
//!      vice/src/snapshot.c, c64/c64-snapshot.c, the per-chip _snapshot.c
//!
//! c64re-own module byte counts (save path):
//!   MAINCPU   11 bytes
//!   C64MEM    65550 bytes
//!   CIA1      48 bytes
//!   CIA2      48 bytes
//!   SID       32 bytes
//!   DRIVECPU  0 bytes (drive blob deferred — Spec 704 §11 R3)
//!   IECBUS    6 bytes
//!   VIC-II    108 bytes
//!   KEYBOARD  6 bytes

use crate::cart::{CartState, FlashCartState};
use crate::cia::CIA_ICR;
use crate::flash040::Flash040SnapState;
use crate::Machine;

// ── VSF header constants ──────────────────────────────────────────────────────

/// VICE Snapshot magic: "VICE Snapshot File\x1A" (19 bytes).
const VSF_MAGIC: &[u8; 19] = b"VICE Snapshot File\x1a";
/// File version.
const VSF_MAJOR: u8 = 2;
const VSF_MINOR: u8 = 0;
/// Machine name (null-terminated).
const VSF_MACHINE: &[u8] = b"C64\0";

/// Module version (all modules use 1.0).
const MOD_MAJOR: u8 = 1;
const MOD_MINOR: u8 = 0;

/// Marker present in VICE x64sc snapshots (SIDEXTENDED module name).
const VICE_MARKER: &[u8] = b"SIDEXTENDED";

/// The "VICE Version\x1a" string in a real VICE snapshot's 58-byte header (Spec
/// 791.4) — a structural fingerprint c64re-own snapshots never carry.
const VICE_VERSION_MARKER: &[u8] = b"VICE Version\x1a";
/// Offset of `VICE_VERSION_MARKER` inside the 58-byte real-VICE header:
/// magic(19) + major/minor(2) + machine_name[16] = 37.
const VICE_VERSION_OFF: usize = 37;

// ── Public types ──────────────────────────────────────────────────────────────

/// Result of a VSF load operation.
#[derive(Debug)]
pub struct VsfLoadResult {
    pub loaded_modules: Vec<String>,
    pub ignored_modules: Vec<String>,
    pub errors: Vec<(String, String)>,
    pub source: &'static str,
}

// ── Fidelity classification (Spec 791.3 — retire the `errors=[]` footgun) ────────

/// How faithfully a VSF was restored into the `Machine`. Replaces the old
/// `errors=[]` signal, which let a caller mistake an inspection-only import for a
/// resumable machine (issue report 2026-07-15). A converter/caller reads THIS to
/// know whether the imported state actually resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fidelity {
    /// Every machine-state-core module (CPU/RAM/CIA/VIC/SID) fully restored and no
    /// *critical* module (`C64CART`, the drive) present-but-dropped ⇒ resumes 1:1.
    Faithful,
    /// The machine-state core is restored (resumable) but at least one critical
    /// module present in the file was NOT restored (e.g. `DRIVE8`, `C64CART`) or a
    /// module came in only approximately (`coarse`) ⇒ resumes, but not 1:1.
    Partial,
    /// The resumable core (CPU + RAM) did not come in — only inspectable registers.
    /// The state can be READ but not reliably run forward.
    InspectionOnly,
}

impl Fidelity {
    /// The Spec 791.3 wire vocabulary (`faithful` | `partial` | `inspection-only`),
    /// used by `trx64cli convert-vsf --json` and text output.
    pub fn as_str(self) -> &'static str {
        match self {
            Fidelity::Faithful => "faithful",
            Fidelity::Partial => "partial",
            Fidelity::InspectionOnly => "inspection-only",
        }
    }
}

/// Per-module honesty report for a VSF load (Spec 791.3). Walks ALL modules present
/// in the file and buckets each: `loaded` (fully restored), `coarse` (restored only
/// approximately — none in slice 1), `absent` (present in the file but NOT restored).
/// `fidelity` is derived from those buckets by [`classify_fidelity`].
#[derive(Debug, Clone)]
pub struct VsfLoadReport {
    pub loaded: Vec<String>,
    pub coarse: Vec<String>,
    pub absent: Vec<String>,
    pub fidelity: Fidelity,
    /// "c64re" | "vice-x64sc" — which framing the file used.
    pub source: &'static str,
}

/// Modules whose absence (present-in-file but NOT restored) means the import is NOT
/// faithful: the cartridge and the 1541 drive. Dropping any of these silently is the
/// exact footgun that turned a partial EF-debug import into a phantom "execution
/// divergence" (issue report 2026-07-15). The VIC micro-pipeline is handled by the
/// coarse-VIC cut (Spec 791.1b) and is NOT listed here in slice 1 — the register
/// head we do lift is a genuine (loaded) restore for resume.
const CRITICAL_MODULES: &[&str] = &["C64CART", "DRIVE8", "DRIVECPU0"];

/// The machine-state-core modules that must be fully restored for a `Faithful`
/// verdict (Spec 791.3). `MAINCPU` + `C64MEM` alone make the machine *resumable*;
/// the rest complete the core.
const CORE_MODULES: &[&str] = &["MAINCPU", "C64MEM", "CIA1", "CIA2", "SID", "VIC-II"];

/// Derive the [`Fidelity`] from the per-module buckets (Spec 791.3 rule):
/// - resumable core (`MAINCPU` + `C64MEM`) absent ⇒ `InspectionOnly`;
/// - any critical module (`CRITICAL_MODULES`) present-but-absent, or any `coarse`
///   module ⇒ `Partial`;
/// - full machine-state core restored and nothing critical missing ⇒ `Faithful`.
fn classify_fidelity(loaded: &[String], coarse: &[String], absent: &[String]) -> Fidelity {
    let in_loaded = |name: &str| loaded.iter().any(|m| m == name);
    // Resumable iff CPU + RAM came in.
    if !in_loaded("MAINCPU") || !in_loaded("C64MEM") {
        return Fidelity::InspectionOnly;
    }
    let critical_dropped = absent.iter().any(|m| CRITICAL_MODULES.contains(&m.as_str()));
    if critical_dropped || !coarse.is_empty() {
        return Fidelity::Partial;
    }
    if CORE_MODULES.iter().all(|c| in_loaded(c)) {
        Fidelity::Faithful
    } else {
        Fidelity::Partial
    }
}

// ── Write helpers ─────────────────────────────────────────────────────────────

#[inline]
fn push_u16_le(buf: &mut Vec<u8>, v: u16) {
    buf.push((v & 0xff) as u8);
    buf.push((v >> 8) as u8);
}

#[inline]
fn push_u32_le(buf: &mut Vec<u8>, v: u32) {
    buf.push((v & 0xff) as u8);
    buf.push(((v >> 8) & 0xff) as u8);
    buf.push(((v >> 16) & 0xff) as u8);
    buf.push(((v >> 24) & 0xff) as u8);
}

/// Write a VSF module: null-terminated name, major, minor, 4-byte LE data length,
/// then data bytes.
fn write_module(buf: &mut Vec<u8>, name: &[u8], data: &[u8]) {
    // Null-terminated module name.
    buf.extend_from_slice(name);
    buf.push(0u8);
    buf.push(MOD_MAJOR);
    buf.push(MOD_MINOR);
    push_u32_le(buf, data.len() as u32);
    buf.extend_from_slice(data);
}

// ── Module serializers ────────────────────────────────────────────────────────

/// MAINCPU module (11 bytes).
/// pc_lo, pc_hi, a, x, y, sp, flags, cycles[4 LE u32]
fn ser_maincpu(machine: &Machine) -> Vec<u8> {
    let cpu = &machine.cpu6510;
    let pc = cpu.reg_pc;
    let flags = cpu.flags();
    let cycles = cpu.clk as u32; // wraps at 4G
    let mut data = Vec::with_capacity(11);
    data.push((pc & 0xff) as u8);
    data.push((pc >> 8) as u8);
    data.push(cpu.reg_a);
    data.push(cpu.reg_x);
    data.push(cpu.reg_y);
    data.push(cpu.reg_sp);
    data.push(flags);
    push_u32_le(&mut data, cycles);
    debug_assert_eq!(data.len(), 11);
    data
}

/// C64MEM module (65550 bytes).
/// ram[65536] + cpuPortDirection[1] + cpuPortValue[1] + dataSetBit6[1] +
/// dataSetBit7[1] + dataSetClkBit6[4 LE] + dataSetClkBit7[4 LE] +
/// dataFalloffBit6[1] + dataFalloffBit7[1]
fn ser_c64mem(machine: &Machine) -> Vec<u8> {
    let mut data = Vec::with_capacity(65550);
    data.extend_from_slice(machine.ram.as_ref());
    data.push(machine.port_dir);
    data.push(machine.port_data);
    data.push(0u8); // dataSetBit6
    data.push(0u8); // dataSetBit7
    push_u32_le(&mut data, 0); // dataSetClkBit6
    push_u32_le(&mut data, 0); // dataSetClkBit7
    data.push(0u8); // dataFalloffBit6
    data.push(0u8); // dataFalloffBit7
    debug_assert_eq!(data.len(), 65550);
    data
}

/// CIA module (48 bytes) — 1:1 with c64re module-mapping.ts `serializeCia`.
///
/// Field order + offsets (module-mapping.ts lines 256-288):
///   c_cia[16]        0..15
///   irqflags         16
///   ack_irqflags     17
///   new_irqflags     18
///   irq_enabled      19
///   rdi[4 LE]        20..23
///   ifr_clock[4 LE]  24..27
///   ifr_delay        28
///   tat              29
///   tbt              30
///   old_pa           31
///   old_pb           32
///   read_clk[4 LE]   33..36
///   read_offset      37
///   last_read        38
///   write_offset     39
///   model            40
///   ta_alarmclk[4]   41..44
///   tb_alarmclk[4]   45..48 (only 45..47 fit — the c64re TS `new Uint8Array(48)`
///                            silently drops the 48th byte; we replicate that
///                            exact truncation so the bytes round-trip 1:1.)
///
/// `old_pa`/`old_pb` = 0xff at reset (VICE bug #1143 — cia6526-vice.ts:416-418;
/// the last byte sent to the port backend, which powers up all-high, NOT the
/// register value). TRX64's `Cia` does not separately track the last port output,
/// so we emit 0xff to match c64re; on load c64re re-derives it on the first port
/// access, so it is non-load-bearing for resume.
/// `model` = 0 (CIA_MODEL_6526, cia6526-vice.ts:169/364 default — the session's
/// CIA1/CIA2 use the default model).
fn ser_cia(cia: &crate::cia::Cia, clk: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(48);
    // c_cia[16] = register file
    data.extend_from_slice(&cia.regs[0..16]);
    // irqflags
    data.push(cia.irqflags);
    // ack_irqflags, new_irqflags
    data.push(0u8);
    data.push(0u8);
    // irq_enabled = ICR mask byte (cia.regs[CIA_ICR])
    data.push(cia.regs[CIA_ICR]);
    // rdi (4 LE)
    push_u32_le(&mut data, 0);
    // ifr_clock = clk as u32
    push_u32_le(&mut data, clk as u32);
    // ifr_delay
    data.push(0u8);
    // tat = 1 if TA running
    data.push(cia.ta.is_running() as u8);
    // tbt = 1 if TB running
    data.push(cia.tb.is_running() as u8);
    // old_pa, old_pb = 0xff (VICE bug #1143; cia6526-vice.ts:416-418).
    data.push(0xff);
    data.push(0xff);
    // read_clk = clk as u32
    push_u32_le(&mut data, clk as u32);
    // read_offset, last_read, write_offset
    data.push(0u8);
    data.push(0u8);
    data.push(0u8); // write_offset = 0 (C64SC; cia6526-vice.ts:235)
    // model = 0 (CIA_MODEL_6526).
    data.push(0u8);
    // ta_alarmclk = the cached next-underflow clk (CLOCK_NEVER=0xffff_ffff_ffff_ffff
    // when stopped). Low 32 bits, matching c64re's `ta_alarmclk` u32 write.
    push_u32_le(&mut data, cia.ta_alarmclk as u32);
    // tb_alarmclk — its 4th byte (data[48]) is dropped: the c64re TS buffer is 48
    // bytes, so writeU32LE at off 45 only stores indices 45..47. We push 4 bytes
    // then truncate the Vec back to 48 to match byte-for-byte.
    push_u32_le(&mut data, cia.tb_alarmclk as u32);
    data.truncate(48);
    debug_assert_eq!(data.len(), 48);
    data
}

/// SID module (32 bytes).
fn ser_sid(machine: &Machine) -> Vec<u8> {
    machine.sid_regs[0..32].to_vec()
}

/// DRIVECPU module — the full 1541 drive CORE snapshot blob.
///
/// 1:1 with c64re session-vsf.ts:116-118, which embeds `drive1541.snapshot()` (the
/// drive-core blob: DRIVE8 + DRIVECPU0 + 1541VIA1D0 + VIA2D0, save_disks=0). We reuse
/// the exact byte-compatible serializer the `.c64re` checkpoint ring already uses
/// (`drive_snapshot::capture_drive1541`), so the VSF DRIVECPU module carries the live
/// drive state instead of an empty stub. The mutable disk image (GCRIMAGE) is NOT
/// embedded in VSF — c64re keeps the VSF DRIVECPU module to the drive CORE only.
fn ser_drivecpu(machine: &mut Machine) -> Vec<u8> {
    crate::drive_snapshot::capture_drive1541(&mut machine.drive8)
}

/// IECBUS module (6 bytes).
/// c64AtnReleased, c64ClkReleased, c64DataReleased,
/// driveClkReleased, driveDataReleased, driveAtnAckReleased
fn ser_iecbus(machine: &Machine) -> Vec<u8> {
    let iec = &machine.iec;
    let c64atn = if (iec.iecbus.cpu_bus & 0x10) != 0 { 1u8 } else { 0u8 };
    let c64clk = if (iec.iecbus.cpu_bus & 0x40) != 0 { 1u8 } else { 0u8 };
    let c64data = if (iec.iecbus.cpu_bus & 0x80) != 0 { 1u8 } else { 0u8 };
    // Drive bus: drv_bus[8] bit6=CLK, bit7=DATA
    let drv_clk = if (iec.iecbus.drv_bus[8] & 0x40) != 0 { 1u8 } else { 0u8 };
    let drv_data = if (iec.iecbus.drv_bus[8] & 0x80) != 0 { 1u8 } else { 0u8 };
    // ATN ACK: use drv_data[8] bit4
    let drv_atn_ack = if (iec.iecbus.drv_data[8] & 0x10) != 0 { 1u8 } else { 0u8 };
    vec![c64atn, c64clk, c64data, drv_clk, drv_data, drv_atn_ack]
}

/// VIC-II module (108 bytes).
/// regs[80] + irq_status[1] + raster_irq_line[2 LE] + raster_irq_clk[4 LE]
/// + allow_bad_lines[1] + bad_line[1] + raster_y[2 LE] + raster_cycle[1]
/// + sprite_fetch_msk[1] + last_read[1] + vbank_phi1[1] + vbank_phi2[1]
/// + screen_ptr[4 LE] + chargen_ptr[4 LE] + bitmap_ptr[4 LE]
fn ser_vicii(machine: &Machine) -> Vec<u8> {
    let vic = &machine.vic;
    let mut data = Vec::with_capacity(108);
    // regs[80]: vic.regs is [u8; 0x40] = 64 bytes; pad to 80 with zeros.
    data.extend_from_slice(&vic.regs[0..64]);
    data.extend_from_slice(&[0u8; 16]); // pad to 80
    // irq_status: the VIC IRQ latch (low 4 bits). The verbatim viciisc VIC keeps
    // this in a dedicated `irq_status` field (not regs[0x19]).
    data.push(vic.irq_status & 0x0f);
    // raster_irq_line: 2 LE
    push_u16_le(&mut data, vic.raster_irq_line);
    // raster_irq_clk: 4 LE (internal, not tracked — use 0)
    push_u32_le(&mut data, 0);
    // allow_bad_lines, bad_line
    data.push(vic.allow_bad_lines as u8);
    data.push(vic.bad_line as u8);
    // raster_y: 2 LE
    push_u16_le(&mut data, vic.raster_line);
    // raster_cycle
    data.push(vic.raster_cycle as u8);
    // sprite_fetch_msk, last_read, vbank_phi1, vbank_phi2
    data.push(0u8);
    data.push(0u8);
    data.push(0u8);
    data.push(0u8);
    // Derive screen_ptr/chargen_ptr/bitmap_ptr from $D018.
    let d018 = vic.regs[0x18];
    let screen_ptr = (((d018 >> 4) & 0xf) as u32) << 10;
    let chargen_ptr = (((d018 >> 1) & 7) as u32) << 11;
    let bitmap_ptr: u32 = if d018 & 8 != 0 { 0x2000 } else { 0 };
    push_u32_le(&mut data, screen_ptr);
    push_u32_le(&mut data, chargen_ptr);
    push_u32_le(&mut data, bitmap_ptr);
    debug_assert_eq!(data.len(), 108);
    data
}

/// KEYBOARD module (6 bytes).
/// cycleNow[4 LE u32] + eventCount[2 LE u16]
fn ser_keyboard(machine: &Machine) -> Vec<u8> {
    let mut data = Vec::with_capacity(6);
    push_u32_le(&mut data, machine.clk as u32);
    push_u16_le(&mut data, 0); // no events
    debug_assert_eq!(data.len(), 6);
    data
}

// ── Public save function ──────────────────────────────────────────────────────

/// Serialize machine state to VSF bytes.
///
/// `&mut Machine` because the DRIVECPU module embeds the live 1541 drive snapshot
/// (`drive_snapshot::capture_drive1541`, which advances the drive's snapshot-sync
/// bookkeeping — a `&mut Drive1541`), exactly as c64re's saveSessionVsf embeds
/// `drive1541.snapshot()`.
pub fn save_vsf(machine: &mut Machine) -> Vec<u8> {
    let mut buf = Vec::with_capacity(70_000);

    // VSF file header.
    buf.extend_from_slice(VSF_MAGIC);
    buf.push(VSF_MAJOR);
    buf.push(VSF_MINOR);
    buf.extend_from_slice(VSF_MACHINE);

    // Capture the drive-core blob up front (needs the mutable borrow) so the rest of
    // the modules can use the shared immutable serializers.
    let drivecpu = ser_drivecpu(machine);

    // Module order MUST match TS: MAINCPU, C64MEM, CIA1, CIA2, SID, DRIVECPU,
    // IECBUS, VIC-II, KEYBOARD.
    write_module(&mut buf, b"MAINCPU", &ser_maincpu(machine));
    write_module(&mut buf, b"C64MEM", &ser_c64mem(machine));

    let clk = machine.clk;
    write_module(&mut buf, b"CIA1", &ser_cia(&machine.cia1, clk));
    write_module(&mut buf, b"CIA2", &ser_cia(&machine.cia2, clk));

    write_module(&mut buf, b"SID", &ser_sid(machine));
    write_module(&mut buf, b"DRIVECPU", &drivecpu);
    write_module(&mut buf, b"IECBUS", &ser_iecbus(machine));
    write_module(&mut buf, b"VIC-II", &ser_vicii(machine));
    write_module(&mut buf, b"KEYBOARD", &ser_keyboard(machine));

    buf
}

// ── Read helpers ──────────────────────────────────────────────────────────────

fn read_u16_le(data: &[u8], off: usize) -> Option<u16> {
    if off + 1 >= data.len() { return None; }
    Some((data[off] as u16) | ((data[off + 1] as u16) << 8))
}

fn read_u32_le(data: &[u8], off: usize) -> Option<u32> {
    if off + 3 >= data.len() { return None; }
    Some((data[off] as u32)
        | ((data[off + 1] as u32) << 8)
        | ((data[off + 2] as u32) << 16)
        | ((data[off + 3] as u32) << 24))
}

/// Read a full little-endian u64 (8 bytes). Used for VICE's 8-byte `SMW_CLOCK`
/// MAINCPU clock (Spec 791.1 — import the WHOLE clock, not just the low 32 bits).
fn read_u64_le(data: &[u8], off: usize) -> Option<u64> {
    let lo = read_u32_le(data, off)? as u64;
    let hi = read_u32_le(data, off + 4)? as u64;
    Some(lo | (hi << 32))
}

// ── Module loaders ────────────────────────────────────────────────────────────

fn load_maincpu(machine: &mut Machine, data: &[u8]) -> Result<(), String> {
    if data.len() < 11 {
        return Err(format!("MAINCPU: expected 11 bytes, got {}", data.len()));
    }
    let pc = (data[0] as u16) | ((data[1] as u16) << 8);
    let a = data[2];
    let x = data[3];
    let y = data[4];
    let sp = data[5];
    let flags = data[6];

    let cpu = &mut machine.cpu6510;
    cpu.reg_pc = pc;
    cpu.reg_a = a;
    cpu.reg_x = x;
    cpu.reg_y = y;
    cpu.reg_sp = sp;
    // Restore P register using the same pattern as the monitor handler.
    cpu.reg_p = flags & !0xa2;
    cpu.flag_n = flags & 0x80;
    cpu.flag_z = if flags & 0x02 != 0 { 0 } else { 1 };

    // Restore clock (wrapping u32 → u64; we just use it as-is since VSF
    // has no high bits — the absolute clock is not preserved across save/load).
    let cycles = read_u32_le(data, 7).map(|c| c as u64);
    if let Some(c) = cycles {
        cpu.clk = c;
    }

    // ── Seed the VERBATIM SC core (`c64_core`) — the PRODUCTION full-machine CPU
    // that `run_for_full*` executes against (lib.rs:1127 reads `c64_core.reg_pc`).
    // The legacy `sync` path only flows c64_core → cpu6510 (sync_snapshot_sc), so
    // a restore that touched ONLY cpu6510 would be ignored the moment the session
    // resumes (the next run reads the stale c64_core PC). Mirror every register +
    // the composite status + the clock into c64_core so the resume continues from
    // the restored CPU state. ──
    let core = &mut machine.c64_core;
    core.reg_pc = pc;
    core.reg_a = a;
    core.reg_x = x;
    core.reg_y = y;
    core.reg_sp = sp;
    core.set_status_composite(flags);
    if let Some(c) = cycles {
        core.clk = c;
    }
    Ok(())
}

fn load_c64mem(machine: &mut Machine, data: &[u8]) -> Result<(), String> {
    if data.len() < 65538 {
        return Err(format!("C64MEM: expected >=65538 bytes, got {}", data.len()));
    }
    machine.ram[0..65536].copy_from_slice(&data[0..65536]);
    machine.port_dir = data[65536];
    machine.port_data = data[65537];
    // Recompute memconfig from the restored port latches.
    let port = (!machine.port_dir | machine.port_data) & 0x07;
    machine.memconfig = machine.memconfig_table[(port | 0x18) as usize & 0x1f];
    Ok(())
}

fn load_cia(cia: &mut crate::cia::Cia, data: &[u8], name: &str) -> Result<(), String> {
    if data.len() < 48 {
        return Err(format!("{name}: expected 48 bytes, got {}", data.len()));
    }
    // c_cia[16] = register file (offsets 0..15).
    cia.regs[0..16].copy_from_slice(&data[0..16]);
    // irqflags (offset 16).
    cia.irqflags = data[16];
    // irq_enabled (offset 19) → ICR mask.
    cia.regs[CIA_ICR] = data[19];
    // Restore timer A/B latches from the TAL/TAH/TBL/TBH registers.
    let tal = cia.regs[crate::cia::CIA_TAL] as u16;
    let tah = cia.regs[crate::cia::CIA_TAH] as u16;
    cia.ta.latch = tal | (tah << 8);
    cia.ta.cnt = cia.ta.latch;
    let tbl = cia.regs[crate::cia::CIA_TBL] as u16;
    let tbh = cia.regs[crate::cia::CIA_TBH] as u16;
    cia.tb.latch = tbl | (tbh << 8);
    cia.tb.cnt = cia.tb.latch;
    // Restore CIA clock from read_clk (offsets 33..36).
    if let Some(clk32) = read_u32_le(data, 33) {
        cia.clk = clk32 as u64;
        cia.ta.clk = clk32 as u64;
        cia.tb.clk = clk32 as u64;
    }
    // Restore cached alarm clocks (offsets 41..44 = ta_alarmclk; 45..47 = the
    // truncated tb_alarmclk — its top byte was dropped on save to match c64re's
    // 48-byte buffer). 0xffff_ffff (CLOCK_NEVER low word) ⇒ map to the full u64
    // CLOCK_NEVER so the alarm-dispatch cascade treats the timer as stopped.
    if let Some(ta32) = read_u32_le(data, 41) {
        cia.ta_alarmclk = widen_alarmclk(ta32);
    }
    // tb_alarmclk: only 3 bytes survive (45..47); reconstruct as if the 4th byte
    // were the save-time truncation. We read the 3 available bytes and treat
    // 0x00ff_ffff (= a stopped timer whose top byte was lost) as CLOCK_NEVER too.
    let tb_lo24 = (data[45] as u32) | ((data[46] as u32) << 8) | ((data[47] as u32) << 16);
    cia.tb_alarmclk = if tb_lo24 == 0x00ff_ffff {
        crate::cia::CLOCK_NEVER
    } else {
        tb_lo24 as u64
    };
    Ok(())
}

/// Widen a 32-bit alarm clock read from a VSF CIA module to the engine's u64
/// alarm clock. A value of 0xffff_ffff (= the low word of CLOCK_NEVER, written
/// when the timer is stopped) maps to the full u64 CLOCK_NEVER.
#[inline]
fn widen_alarmclk(v32: u32) -> u64 {
    if v32 == 0xffff_ffff {
        crate::cia::CLOCK_NEVER
    } else {
        v32 as u64
    }
}

fn load_sid(machine: &mut Machine, data: &[u8]) -> Result<(), String> {
    if data.len() < 32 {
        return Err(format!("SID: expected 32 bytes, got {}", data.len()));
    }
    machine.sid_regs[0..32].copy_from_slice(&data[0..32]);
    // Reset internal voice state — the VSF module is register-file-only (32 bytes);
    // transient oscillator/envelope state is not persisted. Clear to power-on defaults
    // so the SID re-initializes from the restored register file on the next writes.
    machine.sid.reset();
    Ok(())
}

/// DRIVECPU module — restore the 1541 drive CORE from the embedded blob (= c64re
/// session-vsf.ts:217 `drive1541.restore(mod.data)`). An EMPTY module (a legacy save
/// or a save with no live drive) is a clean no-op; otherwise it routes to the same
/// byte-compatible deserializer the `.c64re` checkpoint ring uses.
fn load_drivecpu(machine: &mut Machine, data: &[u8]) -> Result<(), String> {
    if data.is_empty() {
        return Ok(()); // empty stub (legacy/no-drive) — keep the live drive as-is.
    }
    crate::drive_snapshot::restore_drive1541(&mut machine.drive8, data)
}

fn load_iecbus(machine: &mut Machine, data: &[u8]) -> Result<(), String> {
    if data.len() < 6 {
        return Err(format!("IECBUS: expected 6 bytes, got {}", data.len()));
    }
    // Restore cpu_bus from c64AtnReleased/Clk/Data bits.
    let atn_released = data[0] != 0;
    let clk_released = data[1] != 0;
    let data_released = data[2] != 0;
    let iec = &mut machine.iec;
    iec.iecbus.cpu_bus = 0u8
        | if atn_released { 0x10 } else { 0 }
        | if clk_released { 0x40 } else { 0 }
        | if data_released { 0x80 } else { 0 };
    // drv_bus[8] from driveClkReleased/driveDataReleased
    let drv_clk = data[3] != 0;
    let drv_data = data[4] != 0;
    iec.iecbus.drv_bus[8] = 0u8
        | if drv_clk { 0x40 } else { 0 }
        | if drv_data { 0x80 } else { 0 };
    // Update derived ports (= iec_update_ports / c64iec.c:126-138).
    iec.iec_update_ports();
    Ok(())
}

fn load_vicii(machine: &mut Machine, data: &[u8]) -> Result<(), String> {
    if data.len() < 108 {
        return Err(format!("VIC-II: expected 108 bytes, got {}", data.len()));
    }
    let vic = &mut machine.vic;
    // regs[80]: first 64 map to vic.regs; bytes 64..80 are VSF-only extended regs.
    vic.regs[0..64].copy_from_slice(&data[0..64]);
    // irq_status at offset 80 — restore to the dedicated VIC IRQ latch field and
    // recompute the output line level (matching vicii_irq_set_line). The mask
    // ($D01A) is in regs[0x1a] (restored above from data[0..64]).
    vic.irq_status = data[80] & 0x0f;
    vic.irq_line = (vic.irq_status & vic.regs[0x1a] & 0x0f) != 0;
    if vic.irq_line {
        vic.irq_status |= 0x80;
    }
    // raster_irq_line at offset 81 (2 LE).
    if let Some(ril) = read_u16_le(data, 81) {
        vic.raster_irq_line = ril;
    }
    // allow_bad_lines at offset 87, bad_line at 88.
    vic.allow_bad_lines = data[87] != 0;
    vic.bad_line = data[88] != 0;
    // raster_y at offset 89 (2 LE).
    if let Some(ry) = read_u16_le(data, 89) {
        vic.raster_line = ry;
    }
    // raster_cycle at offset 91.
    vic.raster_cycle = data[91] as u16;
    Ok(())
}

fn load_keyboard(_machine: &mut Machine, data: &[u8]) -> Result<(), String> {
    if data.len() < 6 {
        return Err(format!("KEYBOARD: expected 6 bytes, got {}", data.len()));
    }
    // Nothing to restore from the keyboard module (no events).
    Ok(())
}

// ── Real-VICE x64sc snapshot reader ───────────────────────────────────────────
//
// A genuine VICE 3.7+ snapshot uses a DIFFERENT framing from c64re's compact
// format (mirrors C64ReverseEngineeringMCP/.../vsf/vice-vsf-load.ts + VICE
// src/snapshot.c):
//
//   File header (58 bytes, 0x3a):
//     "VICE Snapshot File\x1a"  19 bytes
//     version major / minor      2 bytes
//     machine name               16 bytes (FIXED, zero-padded — e.g. "C64SC")
//     "VICE Version\x1a"         13 bytes
//     vice rc / svn              8 bytes
//   Per module (header = 22 bytes):
//     name                       16 bytes (FIXED, zero-padded)
//     version major / minor      2 bytes
//     size                       4 bytes LE — INCLUDES the 22-byte header
//   Module data: `size - 22` bytes.
//
// We parse the machine-state modules we can map into the Machine (MAINCPU,
// C64MEM, CIA1, CIA2, SID, VIC-II head). The VIC-II module is the 123 KB
// per-cycle VIC-IISC pipeline blob — we take ONLY its model byte + the 64
// public registers + recompute pointers; the pipeline internals are skipped
// (cannot reconstruct without the off-limits viciisc pipeline). The DRIVE*
// modules are skipped (no in-scope game needs the drive resumed from a real
// VICE file). This is enough to RESUME to a sane visible state.

/// Real-VICE module header (location of a module's data within the file).
struct ViceModule {
    data_start: usize,
    data_len: usize,
}

/// Real-VICE file header is 58 bytes; the first module starts at 0x3a.
const VICE_HEADER_LEN: usize = 0x3a;
/// Real-VICE module header is 22 bytes (16 name + maj + min + 4 size).
const VICE_MOD_HEADER_LEN: usize = 22;
const VICE_MOD_NAME_LEN: usize = 16;

/// Find a module by name in a real-VICE file (linear walk over the module list).
fn vice_find_module(data: &[u8], name: &str) -> Option<ViceModule> {
    let mut off = VICE_HEADER_LEN;
    while off + VICE_MOD_HEADER_LEN <= data.len() {
        // Read the fixed 16-byte name field (zero-padded).
        let raw = &data[off..off + VICE_MOD_NAME_LEN];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(VICE_MOD_NAME_LEN);
        let mname = std::str::from_utf8(&raw[..end]).unwrap_or("");
        let size = read_u32_le(data, off + 18)? as usize;
        if size < VICE_MOD_HEADER_LEN {
            break; // malformed — guard against a zero/garbage size loop
        }
        if mname == name {
            return Some(ViceModule {
                data_start: off + VICE_MOD_HEADER_LEN,
                data_len: size - VICE_MOD_HEADER_LEN,
            });
        }
        off += size;
    }
    None
}

/// Detect a genuine VICE x64sc snapshot by HEADER STRUCTURE (Spec 791.4), not only
/// by grepping for the "SIDEXTENDED" module-name string. A real VICE VSF carries the
/// 58-byte header with "VICE Version\x1a" at offset 37; the SIDEXTENDED marker is
/// kept as a fast-path OR (cheap early-out + older detector).
fn is_native_vice_vsf(data: &[u8]) -> bool {
    // Fast path: the SIDEXTENDED module name (c64re never writes it).
    if data.windows(VICE_MARKER.len()).any(|w| w == VICE_MARKER) {
        return true;
    }
    // Structural: VSF magic + the 58-byte-header "VICE Version" marker at offset 37.
    data.len() >= VICE_VERSION_OFF + VICE_VERSION_MARKER.len()
        && data.len() >= VSF_MAGIC.len()
        && &data[0..VSF_MAGIC.len()] == VSF_MAGIC
        && &data[VICE_VERSION_OFF..VICE_VERSION_OFF + VICE_VERSION_MARKER.len()] == VICE_VERSION_MARKER
}

/// Walk the real-VICE module list and return EVERY module name present (Spec 791.3
/// "walk ALL modules"). Mirrors `vice_find_module`'s size-includes-header walk.
fn vice_walk_modules(data: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut off = VICE_HEADER_LEN;
    while off + VICE_MOD_HEADER_LEN <= data.len() {
        let raw = &data[off..off + VICE_MOD_NAME_LEN];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(VICE_MOD_NAME_LEN);
        let mname = std::str::from_utf8(&raw[..end]).unwrap_or("").to_string();
        let size = match read_u32_le(data, off + 18) {
            Some(s) => s as usize,
            None => break,
        };
        if size < VICE_MOD_HEADER_LEN {
            break; // malformed — same guard as vice_find_module
        }
        if !mname.is_empty() {
            names.push(mname);
        }
        off += size;
    }
    names
}

// ── C64CART EasyFlash restore (Spec 791.1a slice 2 — the EF-unblock) ────────────
//
// A real VICE EasyFlash VSF carries THREE modules (easyflash.c / flash040core.c):
//
//   "C64CART"    (c64carthooks.c:3326) — the GENERIC cart-config chunk. Carries
//                `mem_cartridge_type` (= 32 CARTRIDGE_EASYFLASH) + the export lines +
//                the active cart-id list. We do not need its body to rebuild the
//                mapper — the EF-specific "CARTEF" module carries the continuation —
//                but its presence is what the fidelity report keys "C64CART" on.
//   "CARTEF" 0.0 (easyflash.c:677-712):
//                BYTE  jumper
//                BYTE  register_00  (the live 6-bit ROM bank)
//                BYTE  register_02  (mode & 0x87)
//                ARRAY ram[256]     (the $DF00 IO2 RAM)
//                ARRAY roml[0x80000] (lo flash = 64 ROML banks × 8K)
//                ARRAY romh[0x80000] (hi flash = 64 ROMH banks × 8K)
//   "FLASH040EF" 2.0 (flash040core.c:515-538) — TWICE, lo then hi:
//                BYTE  state / BYTE base_state / BYTE program_byte /
//                ARRAY erase_mask[8] / BYTE last_read
//
// Reconstruction: synthesize an EasyFlash `.crt` from the two flash arrays, attach it
// through the normal `attach_cart_from_bytes` path (so the SAME `.crt` bytes end up in
// `cartridge_image.raw_bytes` and the `.c64re` checkpoint's `cartBytes` can rebuild the
// mapper on undump), overlay the EXACT flash bytes via `set_writable_image`, then apply
// the continuation state (bank / register_02 / jumper / IO2-RAM / the lo+hi command
// FSMs) via `set_state`. Field mapping onto `EasyFlashMapper`/`Flash040`:
//   register_00 → current_bank; register_02 → register02; jumper → jumper;
//   ram → io_ram; roml/romh → lo_flash.data/hi_flash.data; the FLASH040EF FSM →
//   Flash040 {state, base_state, program_byte, erase_mask, last_read}.

/// 256-byte EasyFlash IO2 ($DF00) RAM (VICE `CART_RAM_SIZE`).
const EF_CART_RAM_SIZE: usize = 256;
/// One EasyFlash flash chip = 64 banks × 8K = 512 KiB (VICE `roml_banks`/`romh_banks`).
const EF_FLASH_SIZE: usize = 0x80000;
/// VICE `FLASH040_ERASE_MASK_SIZE`.
const EF_ERASE_MASK_SIZE: usize = 8;

/// Find the `nth` (0-based) occurrence of a module by name. VICE writes TWO modules
/// with the SAME name "FLASH040EF" (lo then hi), so `vice_find_module` (first-match)
/// cannot distinguish them.
fn vice_find_module_nth(data: &[u8], name: &str, nth: usize) -> Option<ViceModule> {
    let mut off = VICE_HEADER_LEN;
    let mut count = 0;
    while off + VICE_MOD_HEADER_LEN <= data.len() {
        let raw = &data[off..off + VICE_MOD_NAME_LEN];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(VICE_MOD_NAME_LEN);
        let mname = std::str::from_utf8(&raw[..end]).unwrap_or("");
        let size = read_u32_le(data, off + 18)? as usize;
        if size < VICE_MOD_HEADER_LEN {
            break;
        }
        if mname == name {
            if count == nth {
                return Some(ViceModule {
                    data_start: off + VICE_MOD_HEADER_LEN,
                    data_len: size - VICE_MOD_HEADER_LEN,
                });
            }
            count += 1;
        }
        off += size;
    }
    None
}

/// Is the 8K bank slice at `off` all-0xFF (an unpopulated flash bank)?
fn ef_slice_all_ff(data: &[u8], off: usize) -> bool {
    let end = (off + 0x2000).min(data.len());
    off >= data.len() || data[off..end].iter().all(|&b| b == 0xff)
}

/// Append a CRT `CHIP` packet (16-byte header + `data`) laid out exactly as
/// `parse_crt` reads it (chip type FLASH=2, then bank / load-addr / size, all BE).
fn ef_push_chip(out: &mut Vec<u8>, bank: u16, load_addr: u16, data: &[u8]) {
    out.extend_from_slice(b"CHIP");
    out.extend_from_slice(&((0x10 + data.len()) as u32).to_be_bytes()); // total packet len
    out.extend_from_slice(&2u16.to_be_bytes()); // chip type: FLASH
    out.extend_from_slice(&bank.to_be_bytes()); // bank
    out.extend_from_slice(&load_addr.to_be_bytes()); // load address
    out.extend_from_slice(&(data.len() as u16).to_be_bytes()); // size
    out.extend_from_slice(data);
}

/// Synthesize an EasyFlash `.crt` (hardware type 32) from the two 512 KiB flash
/// arrays. Bank 0 is always emitted (ROML+ROMH) so the mapper has unambiguous
/// geometry; other banks are emitted only when non-empty (byte-exactness is
/// guaranteed by the later `set_writable_image`, so lean synthesis is safe).
fn synth_easyflash_crt(roml: &[u8], romh: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(0x40 + roml.len() + romh.len());
    // ── CRT header (0x40 bytes) ──
    out.extend_from_slice(b"C64 CARTRIDGE   "); // 16
    out.extend_from_slice(&0x40u32.to_be_bytes()); // header length
    out.extend_from_slice(&0x0100u16.to_be_bytes()); // version 1.0
    out.extend_from_slice(&32u16.to_be_bytes()); // hardware type = EasyFlash
    out.push(1); // EXROM (mapper computes live lines from register_02/jumper)
    out.push(1); // GAME
    out.extend_from_slice(&[0u8; 6]); // reserved
    let mut name = [0u8; 32];
    name[..9].copy_from_slice(b"EASYFLASH");
    out.extend_from_slice(&name);
    debug_assert_eq!(out.len(), 0x40);
    // ── CHIP packets ──
    let banks = (roml.len() / 0x2000).max(romh.len() / 0x2000);
    for bank in 0..banks {
        let off = bank * 0x2000;
        if bank == 0 || !ef_slice_all_ff(roml, off) {
            let end = (off + 0x2000).min(roml.len());
            ef_push_chip(&mut out, bank as u16, 0x8000, &roml[off..end]);
        }
        if bank == 0 || !ef_slice_all_ff(romh, off) {
            let end = (off + 0x2000).min(romh.len());
            ef_push_chip(&mut out, bank as u16, 0xa000, &romh[off..end]);
        }
    }
    out
}

/// Parse the `nth` "FLASH040EF" module into a `Flash040SnapState`. VICE does not save
/// the erase-alarm clock; on read it re-arms the alarm to `maincpu_clk +
/// erase_sector_cycles` when the FSM is mid-erase (flash040core.c:571-580). We mirror
/// that: for an AM29F040B (EasyFlash) `erase_sector_cycles = 1_000_000`.
fn parse_flash040ef(data: &[u8], nth: usize, clk: u64) -> Option<Flash040SnapState> {
    let m = vice_find_module_nth(data, "FLASH040EF", nth)?;
    let d = &data[m.data_start..m.data_start + m.data_len];
    if d.len() < 3 + EF_ERASE_MASK_SIZE + 1 {
        return None;
    }
    let state = d[0];
    let base_state = d[1];
    let program_byte = d[2];
    let mut erase_mask = [0u8; EF_ERASE_MASK_SIZE];
    erase_mask.copy_from_slice(&d[3..3 + EF_ERASE_MASK_SIZE]);
    let last_read = d[3 + EF_ERASE_MASK_SIZE];
    // FLASH040_STATE_CHIP_ERASE=9, SECTOR_ERASE=10, SECTOR_ERASE_TIMEOUT=11.
    let erase_alarm_clk = match state {
        9 | 10 | 11 => clk as i64 + 1_000_000,
        _ => -1,
    };
    Some(Flash040SnapState {
        state,
        base_state,
        program_byte,
        last_read,
        dirty: false,
        erase_mask,
        erase_alarm_clk,
    })
}

/// Reconstruct an EasyFlash cart from a VSF's `CARTEF` + `FLASH040EF` modules and
/// attach it to `machine`. Returns `Ok(true)` when an EF cart was restored, `Ok(false)`
/// when the file carries no `CARTEF` (no cart, or a non-EF cart — left absent), and
/// `Err` only on a malformed/short `CARTEF`.
fn load_c64cart_easyflash(machine: &mut Machine, data: &[u8]) -> Result<bool, String> {
    let cartef = match vice_find_module(data, "CARTEF") {
        Some(m) => m,
        None => return Ok(false), // no EasyFlash cart in this file
    };
    let d = &data[cartef.data_start..cartef.data_start + cartef.data_len];
    let need = 3 + EF_CART_RAM_SIZE + 2 * EF_FLASH_SIZE;
    if d.len() < need {
        return Err(format!("CARTEF too short: {} < {}", d.len(), need));
    }
    let jumper = d[0] & 1;
    let register_00 = d[1]; // live ROM bank
    let register_02 = d[2]; // mode & 0x87
    let ram = d[3..3 + EF_CART_RAM_SIZE].to_vec();
    let roml_off = 3 + EF_CART_RAM_SIZE;
    let romh_off = roml_off + EF_FLASH_SIZE;
    let roml = &d[roml_off..roml_off + EF_FLASH_SIZE];
    let romh = &d[romh_off..romh_off + EF_FLASH_SIZE];

    let clk = machine.c64_core.clk;
    let lo_fsm = parse_flash040ef(data, 0, clk);
    let hi_fsm = parse_flash040ef(data, 1, clk);

    // Attach via a synthesized EF `.crt` (reuses the whole attach + `.c64re` cart-embed
    // path), then overlay exact flash bytes + the continuation state.
    let crt = synth_easyflash_crt(roml, romh);
    machine
        .attach_cart_from_bytes(&crt, "vsf-easyflash")
        .map_err(|e| format!("attach EasyFlash: {e:?}"))?;
    if let Some(cart) = machine.cartridge.as_mut() {
        let mut img = Vec::with_capacity(2 * EF_FLASH_SIZE);
        img.extend_from_slice(roml);
        img.extend_from_slice(romh);
        cart.set_writable_image(&img); // byte-exact flash (wins over EAPI-replacement)
        cart.set_state(CartState {
            current_bank: register_00 as u16,
            control_register: Some(register_02),
            flash: Some(FlashCartState {
                flash_lo: lo_fsm,
                flash_hi: hi_fsm,
                eeprom: None,
                easyflash_jumper: jumper,
                easyflash_ram: ram,
            }),
        });
    }
    // Recompute the live memconfig from the restored EF lines (register_02 + jumper
    // drive 8k/16k/off/ultimax — the attach above ran with register_02=0).
    machine.memconfig = machine.memconfig_table[machine.pla_index()];
    Ok(true)
}

/// Parse a real-VICE x64sc .vsf and inject the recoverable machine state into
/// `machine`. Returns the modules that were parsed (loaded) and those skipped.
fn load_vice_vsf(machine: &mut Machine, data: &[u8]) -> Result<VsfLoadResult, String> {
    let mut loaded = Vec::new();
    let mut ignored = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();

    // ── C64MEM (must come before MAINCPU's memconfig is used) ──
    // VICE C64MEM v0.1: pport.data[0], pport.dir[1], exrom[2], game[3], RAM[4..].
    match vice_find_module(data, "C64MEM") {
        Some(m) if m.data_len >= 65536 + 4 => {
            let d = &data[m.data_start..m.data_start + m.data_len];
            let pport_data = d[0];
            let pport_dir = d[1];
            // Copy the 64K RAM verbatim. VICE stores the real DRAM bytes under
            // $00/$01 here; the CPU port latches are a SEPARATE pair (below) that
            // TRX64's read_full returns for $00/$01 — so we must NOT clobber the
            // RAM image with the latch values.
            machine.ram[0..65536].copy_from_slice(&d[4..4 + 65536]);
            machine.port_data = pport_data;
            machine.port_dir = pport_dir;
            let port = (!machine.port_dir | machine.port_data) & 0x07;
            machine.memconfig = machine.memconfig_table[(port | 0x18) as usize & 0x1f];
            loaded.push("C64MEM".to_string());
        }
        Some(m) => errors.push(("C64MEM".into(), format!("too short: {}", m.data_len))),
        None => errors.push(("C64MEM".into(), "missing".into())),
    }

    // ── MAINCPU v1.4: CLK[8], A[8], X[9], Y[10], SP[11], PC[12..13], STATUS[14] ──
    match vice_find_module(data, "MAINCPU") {
        Some(m) if m.data_len >= 15 => {
            let d = &data[m.data_start..m.data_start + m.data_len];
            // CLK is an 8-byte SMW_CLOCK. Spec 791.1 — import the FULL 64-bit clock
            // (TRX64's engine clock is a monotonic JS-safe u64, Spec 743). Previously
            // only the low 32 bits came in, so a snapshot taken past the 32-bit wrap
            // (~72 min of PAL runtime) resumed with a truncated clock baseline.
            let clk = read_u64_le(d, 0).unwrap_or(0);
            let a = d[8];
            let x = d[9];
            let y = d[10];
            let sp = d[11];
            let pc = (d[12] as u16) | ((d[13] as u16) << 8);
            let status = d[14];
            // Seed BOTH the verbatim SC core (production full-machine CPU) and the
            // legacy cpu6510 view (see load_maincpu rationale).
            let core = &mut machine.c64_core;
            core.reg_pc = pc;
            core.reg_a = a;
            core.reg_x = x;
            core.reg_y = y;
            core.reg_sp = sp;
            core.set_status_composite(status);
            core.clk = clk;
            let cpu = &mut machine.cpu6510;
            cpu.reg_pc = pc;
            cpu.reg_a = a;
            cpu.reg_x = x;
            cpu.reg_y = y;
            cpu.reg_sp = sp;
            cpu.reg_p = status & !0xa2;
            cpu.flag_n = status & 0x80;
            cpu.flag_z = if status & 0x02 != 0 { 0 } else { 1 };
            cpu.clk = clk;
            loaded.push("MAINCPU".to_string());
        }
        Some(m) => errors.push(("MAINCPU".into(), format!("too short: {}", m.data_len))),
        None => errors.push(("MAINCPU".into(), "missing".into())),
    }

    // ── CIA1 / CIA2 v2.5 ──
    // VICE order: PRA[0] PRB[1] DDRA[2] DDRB[3] TIMER_A[4..5] TIMER_B[6..7]
    //   TOD_TEN[8] TOD_SEC[9] TOD_MIN[10] TOD_HR[11] SDR[12] ICR[13] CRA[14]
    //   CRB[15] LATCH_A[16..17] LATCH_B[18..19] ...
    let cia_tab = machine.cia_table.clone();
    for (name, want_cia2) in [("CIA1", false), ("CIA2", true)] {
        match vice_find_module(data, name) {
            Some(m) if m.data_len >= 20 => {
                let d = &data[m.data_start..m.data_start + m.data_len];
                let cia = if want_cia2 { &mut machine.cia2 } else { &mut machine.cia1 };
                // Map the register file: the port + DDR + TOD + SDR + control bytes
                // line up 1:1 with TRX64's register indices. The TAL/TAH/TBL/TBH
                // register bytes hold the LATCH (VICE LATCH_A/B), not the live
                // counter (VICE TIMER_A/B, which restores into the counter `cnt`).
                cia.regs[crate::cia::CIA_PRA] = d[0];
                cia.regs[crate::cia::CIA_PRB] = d[1];
                cia.regs[crate::cia::CIA_DDRA] = d[2];
                cia.regs[crate::cia::CIA_DDRB] = d[3];
                let latch_a = (d[16] as u16) | ((d[17] as u16) << 8);
                let latch_b = (d[18] as u16) | ((d[19] as u16) << 8);
                cia.regs[crate::cia::CIA_TAL] = (latch_a & 0xff) as u8;
                cia.regs[crate::cia::CIA_TAH] = (latch_a >> 8) as u8;
                cia.regs[crate::cia::CIA_TBL] = (latch_b & 0xff) as u8;
                cia.regs[crate::cia::CIA_TBH] = (latch_b >> 8) as u8;
                cia.regs[crate::cia::CIA_TOD_TEN] = d[8];
                cia.regs[crate::cia::CIA_TOD_SEC] = d[9];
                cia.regs[crate::cia::CIA_TOD_MIN] = d[10];
                cia.regs[crate::cia::CIA_TOD_HR] = d[11];
                cia.regs[crate::cia::CIA_SDR] = d[12];
                cia.regs[CIA_ICR] = d[13]; // ICR mask
                cia.regs[crate::cia::CIA_CRA] = d[14];
                cia.regs[crate::cia::CIA_CRB] = d[15];
                // Live counter + latch.
                let timer_a = (d[4] as u16) | ((d[5] as u16) << 8);
                let timer_b = (d[6] as u16) | ((d[7] as u16) << 8);
                cia.ta.latch = latch_a;
                cia.ta.cnt = timer_a;
                cia.tb.latch = latch_b;
                cia.tb.cnt = timer_b;
                // Align the chip + timer clocks with the restored CPU clk so timer
                // state machines run from a consistent baseline.
                let clk = machine.c64_core.clk;
                cia.clk = clk;
                cia.ta.clk = clk;
                cia.tb.clk = clk;
                // Re-arm the timer alarms from the restored CRA/CRB + cnt/latch. The
                // register write above set the FILE but not the Ciat control state or
                // the cached underflow-alarm clk (still CLOCK_NEVER from Machine::new),
                // so a running timer would never fire again — the game's frame clock +
                // raster-split IRQ stall (VSF resumed to a garbled bottom split / dead
                // timer). VICE cia_snapshot_read_module does the same re-arm on load.
                cia.restore_rearm_alarms(cia_tab.as_ref());
                loaded.push(name.to_string());
            }
            Some(m) => errors.push((name.into(), format!("too short: {}", m.data_len))),
            None => ignored.push(name.to_string()),
        }
    }

    // ── SID v1.5: num_sids[0] sound[1] engine[2] model[3] sid_registers[4..36] ──
    match vice_find_module(data, "SID") {
        Some(m) if m.data_len >= 4 + 32 => {
            let d = &data[m.data_start..m.data_start + m.data_len];
            machine.sid_regs[0..32].copy_from_slice(&d[4..4 + 32]);
            machine.sid.reset(); // register-file only; clear transient voice state
            loaded.push("SID".to_string());
        }
        Some(_) | None => ignored.push("SID".to_string()),
    }

    // ── VIC-IISC (x64sc) module: model[0] + regs[0x40]@1, then the cycle-exact
    //    micro-pipeline (coarse-VIC cut: we take regs + raster position + the
    //    COLOUR RAM). The colour RAM is the critical non-pipeline field: it is a
    //    SEPARATE 1K chip (not in C64MEM's 64K DRAM), feeds the "11" multicolor
    //    bitmap pattern + text colour, and without it every colour-RAM pixel resolves
    //    to colour 0 (black) — a mid-game EF screen resumed with its white menu/HUD
    //    rendered black. VICE writes it at a fixed offset inside the VIC-IISC module
    //    (viciisc/vicii-snapshot.c: model 1 + regs 64 + raster_cycle/cycle_flags/
    //    raster_line 3×DW + start_of_frame 1 + irq_status 1 + raster_irq_line DW +
    //    raster_irq_triggered 1 + vbuf 40 + cbuf 40 + gbuf 1 + dbuf_offset DW + dbuf
    //    520 + ysmooth DW + 4×B + 6×DW(idle/vcbase/vc/rc/vmli/bad_line) + lp(2×B +
    //    3×DW + CLOCK/qword) + reg11_delay 1 + 2×DW + 9×B → colour_ram @ 761).
    const VICIISC_COLOR_RAM_OFF: usize = 761;
    match vice_find_module(data, "VIC-II").or_else(|| vice_find_module(data, "VIC-IISC")) {
        Some(m) if m.data_len >= 1 + 64 => {
            let d = &data[m.data_start..m.data_start + m.data_len];
            // d[0] = model byte; d[1..65] = the 64 public VIC registers.
            machine.vic.regs[0..64].copy_from_slice(&d[1..1 + 64]);
            // Raster position (viciisc order: raster_cycle, cycle_flags, raster_line —
            // each a DWORD right after the registers) so the resumed frame starts where
            // VICE dumped it, not at line 0.
            let dw = |o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
            if d.len() >= 77 {
                machine.vic.raster_cycle = dw(65) as u16;
                machine.vic.cycle_flags = dw(69);
                machine.vic.raster_line = dw(73) as u16;
            }
            // ysmooth (the latched vertical-scroll, a SEPARATE DWORD @ 689 — NOT derived
            // from regs[$11]&7 by the renderer). Left at 0 it shifts the whole display 7
            // rows vs a dump taken with ysmooth=7 (the observed 7px vertical offset).
            // Offsets after regs: raster_cycle/cycle_flags/raster_line 3×DW + sof 1 +
            // irq_status 1 + raster_irq_line DW + raster_irq_triggered 1 + vbuf 40 +
            // cbuf 40 + gbuf 1 + dbuf_offset DW + dbuf 520 → ysmooth @ 689.
            if d.len() >= 693 {
                machine.vic.ysmooth = dw(689) as u8;
            }
            // Recompute the IRQ line from the restored $D019 latch ∧ $D01A mask.
            machine.vic.irq_status = machine.vic.regs[0x19] & 0x0f;
            machine.vic.irq_line =
                (machine.vic.irq_status & machine.vic.regs[0x1a] & 0x0f) != 0;
            if machine.vic.irq_line {
                machine.vic.irq_status |= 0x80;
            }
            // Raster-IRQ compare line ($D012 + $D011 bit7).
            machine.vic.raster_irq_line =
                (machine.vic.regs[0x12] as u16) | (((machine.vic.regs[0x11] as u16) & 0x80) << 1);
            // COLOUR RAM (0x400, low nibbles) → `write_color_ram` writes BOTH the
            // `ram[$D800..]` and `io_shadow[$0800..]` stores; the full-machine VIC
            // reads colour RAM from `io_shadow`, so without it the "11" multicolor
            // pixels resolve to black (white menu/HUD rendered black on resume).
            if d.len() >= VICIISC_COLOR_RAM_OFF + 0x400 {
                crate::c64re_snapshot::write_color_ram(
                    machine,
                    &d[VICIISC_COLOR_RAM_OFF..VICIISC_COLOR_RAM_OFF + 0x400],
                );
            }
            loaded.push("VIC-II".to_string());
        }
        Some(_) | None => ignored.push("VIC-II".to_string()),
    }

    // ── C64CART (EasyFlash) — Spec 791.1a slice 2 (the EF-unblock) ──
    // A real EF VSF carries the generic "C64CART" module + the EF-specific "CARTEF"
    // (jumper / register_00 / register_02 / IO2-RAM + the two 512K flash arrays) + two
    // "FLASH040EF" command-FSM modules. Reconstruct the EasyFlashMapper and attach it.
    // Non-EF `C64CART` (a "C64CART" chunk with no "CARTEF") is left absent (task pt 4).
    let ef_loaded = match load_c64cart_easyflash(machine, data) {
        Ok(true) => {
            loaded.push("C64CART".to_string());
            loaded.push("CARTEF".to_string());
            loaded.push("FLASH040EF".to_string());
            true
        }
        Ok(false) => false,
        Err(e) => {
            errors.push(("C64CART".into(), e));
            false
        }
    };

    // Restore the IEC bus to its released (idle) baseline — the real-VICE drive
    // modules carry the live bus, but TRX64 does not resume the drive from a VICE
    // file (see header). A released bus is the correct idle state for a paused C64.
    machine.iec.iecbus.cpu_bus = 0x10 | 0x40 | 0x80; // ATN/CLK/DATA all released
    machine.iec.iec_update_ports();

    // Note the DRIVE8/9/10/11, DRIVECPU0, 1541VIA1D0, VIA2D0, FSDRIVE, GLUE,
    // C64MEMHACKS, TAPEPORT, DATASETTE, KEYBOARD, JOYPORT*, JOYSTICK*, USERPORT,
    // SIDEXTENDED, C64CART modules as ignored (not mapped into the Machine) — except a
    // C64CART we restored above (EasyFlash), which is credited to `loaded`.
    for n in [
        "DRIVE8", "DRIVECPU0", "1541VIA1D0", "VIA2D0", "FSDRIVE", "GLUE",
        "TAPEPORT", "DATASETTE", "KEYBOARD", "SIDEXTENDED", "C64CART",
    ] {
        if n == "C64CART" && ef_loaded {
            continue;
        }
        if vice_find_module(data, n).is_some() {
            ignored.push(n.to_string());
        }
    }

    machine.sync_after_monitor();
    machine.clk = machine.c64_core.clk;

    Ok(VsfLoadResult {
        loaded_modules: loaded,
        ignored_modules: ignored,
        errors,
        source: "vice-x64sc",
    })
}

// ── Public load function ──────────────────────────────────────────────────────

/// Restore machine state from VSF bytes.
///
/// Auto-detects a real VICE x64sc snapshot (contains the "SIDEXTENDED" module
/// name, which c64re never writes — Spec 770.2) and routes it to the dedicated
/// real-VICE parser (`load_vice_vsf`). c64re-own snapshots fall through to the
/// compact module-by-module parser.
pub fn load_vsf(machine: &mut Machine, data: &[u8]) -> Result<VsfLoadResult, String> {
    // VICE detection (Spec 791.4): the 58-byte-header "VICE Version" fingerprint +
    // module structure, OR the fast-path "SIDEXTENDED" module name (c64re never
    // writes it). A c64re-own snapshot falls through to the compact parser below.
    if is_native_vice_vsf(data) {
        return load_vice_vsf(machine, data);
    }

    // Parse VSF header.
    if data.len() < 19 + 2 + 4 {
        return Err(format!("VSF: file too small ({} bytes)", data.len()));
    }
    if &data[0..19] != VSF_MAGIC {
        return Err("VSF: magic mismatch (not a VICE Snapshot File)".to_string());
    }
    // Skip major (data[19]) and minor (data[20]).
    // Null-terminated machine name starts at data[21].
    let name_start = 21;
    let name_end = data[name_start..].iter().position(|&b| b == 0)
        .map(|p| name_start + p + 1) // include the null
        .unwrap_or(name_start + 1);
    let mut cursor = name_end;

    let mut loaded = Vec::new();
    let mut ignored = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();

    // Parse modules until EOF.
    while cursor < data.len() {
        // Read null-terminated module name.
        let name_start_m = cursor;
        let name_null = data[cursor..].iter().position(|&b| b == 0);
        let name_null = match name_null {
            Some(p) => cursor + p,
            None => break, // truncated
        };
        let mod_name = std::str::from_utf8(&data[name_start_m..name_null])
            .unwrap_or("?")
            .to_string();
        cursor = name_null + 1;

        // major, minor, length (4 LE).
        if cursor + 6 > data.len() {
            errors.push((mod_name.clone(), "truncated header".into()));
            break;
        }
        let _major = data[cursor];
        let _minor = data[cursor + 1];
        cursor += 2;
        let mod_len = match read_u32_le(data, cursor) {
            Some(v) => v as usize,
            None => {
                errors.push((mod_name.clone(), "truncated length".into()));
                break;
            }
        };
        cursor += 4;

        if cursor + mod_len > data.len() {
            errors.push((mod_name.clone(), format!("data truncated (need {mod_len}, have {})", data.len() - cursor)));
            break;
        }
        let mod_data = &data[cursor..cursor + mod_len];
        cursor += mod_len;

        let result = match mod_name.as_str() {
            "MAINCPU" => load_maincpu(machine, mod_data),
            "C64MEM" => load_c64mem(machine, mod_data),
            "CIA1" => load_cia(&mut machine.cia1, mod_data, "CIA1"),
            "CIA2" => load_cia(&mut machine.cia2, mod_data, "CIA2"),
            "SID" => load_sid(machine, mod_data),
            // DRIVECPU now carries the drive-core blob (= c64re drive1541.restore()).
            // An empty module (a legacy save / no live drive) is a no-op restore.
            "DRIVECPU" => load_drivecpu(machine, mod_data),
            "IECBUS" => load_iecbus(machine, mod_data),
            "VIC-II" => load_vicii(machine, mod_data),
            "KEYBOARD" => load_keyboard(machine, mod_data),
            _ => {
                ignored.push(mod_name.clone());
                continue;
            }
        };

        match result {
            Ok(()) => loaded.push(mod_name),
            Err(e) => errors.push((mod_name, e)),
        }
    }

    // Sync CPU shadow registers.
    machine.sync_after_monitor();
    // Sync machine clk from CPU.
    machine.clk = machine.cpu6510.clk;

    Ok(VsfLoadResult {
        loaded_modules: loaded,
        ignored_modules: ignored,
        errors,
        source: "c64re",
    })
}

// ── Fidelity-reporting load (Spec 791.3 — the converter's entry) ────────────────

/// Walk the c64re-own module list and return EVERY module name present. Mirrors the
/// `load_vsf` compact parse loop (null-terminated name + size EXCLUDES header).
fn c64re_walk_modules(data: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    if data.len() < 19 + 2 + 4 || &data[0..19] != VSF_MAGIC {
        return names;
    }
    let name_start = 21;
    let name_end = data[name_start..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| name_start + p + 1)
        .unwrap_or(name_start + 1);
    let mut cursor = name_end;
    while cursor < data.len() {
        let name_null = match data[cursor..].iter().position(|&b| b == 0) {
            Some(p) => cursor + p,
            None => break,
        };
        let mod_name = std::str::from_utf8(&data[cursor..name_null]).unwrap_or("?").to_string();
        cursor = name_null + 1;
        if cursor + 6 > data.len() {
            break;
        }
        cursor += 2; // major/minor
        let mod_len = match read_u32_le(data, cursor) {
            Some(v) => v as usize,
            None => break,
        };
        cursor += 4;
        if cursor + mod_len > data.len() {
            break;
        }
        cursor += mod_len;
        names.push(mod_name);
    }
    names
}

/// Bucket present modules into loaded/coarse/absent and classify (Spec 791.3).
/// `coarse` is empty in slice 1 (no approximate restores yet — Spec 791.1b adds the
/// coarse VIC). A present module counts as loaded when its (normalized) name is in
/// `loaded`; the real-VICE VIC pipeline module ("VIC-IISC") normalizes to the loaded
/// head "VIC-II", so the register-head restore is NOT double-listed as absent.
fn build_report(source: &'static str, loaded: &[String], present: &[String]) -> VsfLoadReport {
    let loaded: Vec<String> = loaded.to_vec();
    let is_loaded = |name: &str| -> bool {
        loaded.iter().any(|m| m == name)
            || ((name == "VIC-IISC" || name == "VIC-II") && loaded.iter().any(|m| m == "VIC-II"))
    };
    let mut absent: Vec<String> = Vec::new();
    for m in present {
        if !is_loaded(m) && !absent.contains(m) {
            absent.push(m.clone());
        }
    }
    let coarse: Vec<String> = Vec::new();
    let fidelity = classify_fidelity(&loaded, &coarse, &absent);
    VsfLoadReport { loaded, coarse, absent, fidelity, source }
}

/// Load a VSF and return the Spec 791.3 fidelity report (loaded / coarse / absent +
/// `Fidelity`) instead of the bare `errors=[]` list — the honesty-fixed entry the
/// `trx64cli convert-vsf` converter uses. `load_vsf` stays for back-compat callers
/// that only need the restored machine + loaded-module list.
pub fn load_vsf_report(machine: &mut Machine, data: &[u8]) -> Result<VsfLoadReport, String> {
    if is_native_vice_vsf(data) {
        let result = load_vice_vsf(machine, data)?;
        let present = vice_walk_modules(data);
        Ok(build_report(result.source, &result.loaded_modules, &present))
    } else {
        let result = load_vsf(machine, data)?;
        let present = c64re_walk_modules(data);
        Ok(build_report(result.source, &result.loaded_modules, &present))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Machine;

    #[test]
    fn roundtrip_maincpu() {
        let mut m = Machine::new();
        m.cpu6510.reg_pc = 0x1234;
        m.cpu6510.reg_a = 0xAB;
        m.cpu6510.reg_x = 0xCD;
        m.cpu6510.reg_y = 0xEF;
        m.cpu6510.reg_sp = 0x7F;
        m.cpu6510.clk = 50000;
        m.clk = 50000;

        let bytes = save_vsf(&mut m);
        let mut m2 = Machine::new();
        let result = load_vsf(&mut m2, &bytes).expect("load failed");
        assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);

        assert_eq!(m2.cpu6510.reg_pc, 0x1234);
        assert_eq!(m2.cpu6510.reg_a, 0xAB);
        assert_eq!(m2.cpu6510.reg_x, 0xCD);
        assert_eq!(m2.cpu6510.reg_y, 0xEF);
        assert_eq!(m2.cpu6510.reg_sp, 0x7F);
    }

    #[test]
    fn roundtrip_c64mem() {
        let mut m = Machine::new();
        m.ram[0x1000] = 0xDE;
        m.ram[0x2000] = 0xAD;
        m.port_dir = 0x2F;
        m.port_data = 0x37;

        let bytes = save_vsf(&mut m);
        let mut m2 = Machine::new();
        let result = load_vsf(&mut m2, &bytes).expect("load failed");
        assert!(result.errors.is_empty());

        assert_eq!(m2.ram[0x1000], 0xDE);
        assert_eq!(m2.ram[0x2000], 0xAD);
        assert_eq!(m2.port_dir, 0x2F);
        assert_eq!(m2.port_data, 0x37);
    }

    #[test]
    fn vice_marker_routes_to_vice_parser() {
        // A file carrying the "SIDEXTENDED" module name routes to the real-VICE
        // parser (source = "vice-x64sc"), NOT the c64re compact parser. The fake
        // here has no MAINCPU/C64MEM, so those surface as errors — but the routing
        // + source tag is what we assert.
        let mut fake = Vec::new();
        fake.extend_from_slice(VSF_MAGIC);
        fake.push(VSF_MAJOR);
        fake.push(VSF_MINOR);
        fake.extend_from_slice(VSF_MACHINE);
        fake.extend_from_slice(b"SIDEXTENDED\0\x01\x00\x00\x00\x00\x00");
        let mut m = Machine::new();
        let r = load_vsf(&mut m, &fake).expect("vice parser should not hard-error");
        assert_eq!(r.source, "vice-x64sc");
    }

    /// A real VICE x64sc snapshot (samples/motm.vsf) must LOAD and resume to a
    /// sane state. Parses MAINCPU/C64MEM/CIA1/CIA2/SID/VIC-II; the drive + VIC
    /// pipeline modules are skipped.
    #[test]
    fn load_real_vice_motm() {
        // motm.vsf is a copyrighted commercial-game snapshot — NOT tracked in the
        // repo (gitignored). Read at runtime if present; skip cleanly if absent.
        let Ok(motm) = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vsf/motm.vsf")) else {
            eprintln!("SKIP load_real_vice_motm: tests/fixtures/vsf/motm.vsf absent (copyrighted, not in repo)");
            return;
        };
        let motm: &[u8] = &motm;
        let mut m = Machine::new();
        let r = load_vsf(&mut m, motm).expect("load motm.vsf");
        assert_eq!(r.source, "vice-x64sc");
        // The machine-state modules we map must all parse.
        for need in ["MAINCPU", "C64MEM", "CIA1", "CIA2", "SID", "VIC-II"] {
            assert!(
                r.loaded_modules.iter().any(|s| s == need),
                "module {need} not loaded; loaded={:?} errors={:?}",
                r.loaded_modules,
                r.errors
            );
        }
        // motm.vsf MAINCPU has PC=$a892 — a sane RAM/code address, not garbage.
        assert_eq!(m.c64_core.reg_pc, 0xa892, "restored PC");
        assert_eq!(m.cpu6510.reg_pc, 0xa892, "cpu6510 PC mirror");
        // CPU port latch restored (dir = 0x2f standard direction; data = 0x15 in
        // this snapshot ⟹ HIRAM=0 = KERNAL/BASIC banked OUT, game runs from RAM).
        assert_eq!(m.port_dir, 0x2f, "pport.dir");
        assert_eq!(m.port_data, 0x15, "pport.data");
        // PC=$a892 falls in the $A000-$BFFF window; with HIRAM=0 that window is
        // RAM, so the restored PC points at the game's RAM code — a coherent,
        // resumable config (not garbage).
        assert!(!m.memconfig.basic, "BASIC ROM banked out (HIRAM=0)");
        assert!(!m.memconfig.kernal, "KERNAL ROM banked out (HIRAM=0)");

        // RESUME: run the full machine forward a few frames; the PC must stay in a
        // sane range (no jam, no runaway into unmapped space) and the clock must
        // advance — proving the restored state is executable.
        let start_clk = m.clk;
        let mut nop = crate::NullSink;
        m.run_for_full(100_000, &mut nop, |_, _, _, _, _, _, _| {});
        assert!(m.clk > start_clk, "machine clock must advance on resume");
    }

    #[test]
    fn module_byte_counts() {
        let mut m = Machine::new();
        assert_eq!(ser_maincpu(&m).len(), 11, "MAINCPU");
        assert_eq!(ser_c64mem(&m).len(), 65550, "C64MEM");
        assert_eq!(ser_cia(&m.cia1, 0).len(), 48, "CIA");
        assert_eq!(ser_sid(&m).len(), 32, "SID");
        // Fork B (formats-state-1): DRIVECPU is no longer the empty stub — it embeds
        // the full drive-core blob (DRIVE8 + DRIVECPU0 + 1541VIA1D0 + VIA2D0), matching
        // c64re's saveSessionVsf which embeds drive1541.snapshot(). It must be NON-empty.
        assert!(ser_drivecpu(&mut m).len() > 0, "DRIVECPU (drive-core blob)");
        assert_eq!(ser_iecbus(&m).len(), 6, "IECBUS");
        assert_eq!(ser_vicii(&m).len(), 108, "VIC-II");
        assert_eq!(ser_keyboard(&m).len(), 6, "KEYBOARD");
    }

    // ── Spec 791 — synthetic real-VICE VSF builders (no copyrighted sample needed) ─
    //
    // A minimal but structurally-genuine VICE x64sc snapshot: the 58-byte header
    // ("VICE Version\x1a" fingerprint) + 22-byte-per-module framing (size INCLUDES
    // the header). Enough for the loader + the fidelity classifier to exercise their
    // real paths without a copyrighted game snapshot.

    /// The 58-byte real-VICE file header.
    fn vice_header() -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(VSF_MAGIC); // 19
        h.push(2);
        h.push(0); // major/minor
        let mut mach = [0u8; 16];
        mach[..5].copy_from_slice(b"C64SC");
        h.extend_from_slice(&mach); // 16
        h.extend_from_slice(VICE_VERSION_MARKER); // "VICE Version\x1a" (13)
        h.extend_from_slice(&[3, 7, 0, 0, 0, 0, 0, 0]); // rc/svn (8)
        assert_eq!(h.len(), VICE_HEADER_LEN, "real-VICE header is 58 bytes");
        h
    }

    /// Append a real-VICE module (16-byte name, major, minor, 4-byte size that
    /// INCLUDES the 22-byte header, then `data`).
    fn push_vice_module(buf: &mut Vec<u8>, name: &str, data: &[u8]) {
        let mut nm = [0u8; VICE_MOD_NAME_LEN];
        let n = name.len().min(VICE_MOD_NAME_LEN);
        nm[..n].copy_from_slice(&name.as_bytes()[..n]);
        buf.extend_from_slice(&nm);
        buf.push(1);
        buf.push(0); // major/minor
        push_u32_le(buf, (VICE_MOD_HEADER_LEN + data.len()) as u32);
        buf.extend_from_slice(data);
    }

    /// A MAINCPU v1.4 module body: CLK[0..8], A[8], X[9], Y[10], SP[11],
    /// PC[12..14], STATUS[14]. `clk` is the FULL 64-bit SMW_CLOCK.
    fn vice_maincpu(clk: u64, pc: u16) -> Vec<u8> {
        let mut d = Vec::new();
        for i in 0..8 {
            d.push(((clk >> (i * 8)) & 0xff) as u8);
        }
        d.push(0x11); // A
        d.push(0x22); // X
        d.push(0x33); // Y
        d.push(0xfd); // SP
        d.push((pc & 0xff) as u8);
        d.push((pc >> 8) as u8);
        d.push(0x24); // STATUS (I set)
        d
    }

    /// A C64MEM v0.1 module body: pport.data[0], pport.dir[1], exrom[2], game[3],
    /// RAM[4..4+65536]. Places `loop_at`: SEI; JMP loop_at so a resume spins in RAM
    /// (never warm-starts), with the CPU port set to all-RAM ($34) so the loop runs.
    fn vice_c64mem(loop_at: u16) -> Vec<u8> {
        let mut d = vec![0x34, 0x2f, 1, 1]; // pport.data=$34 (all-RAM), dir=$2f, exrom, game
        let mut ram = vec![0u8; 65536];
        let a = loop_at as usize;
        ram[a] = 0x78; // SEI
        ram[a + 1] = 0x4c; // JMP abs
        ram[a + 2] = (loop_at & 0xff) as u8;
        ram[a + 3] = (loop_at >> 8) as u8;
        ram[0x4000] = 0xa5; // a RAM spot-check marker
        d.extend_from_slice(&ram);
        d
    }

    /// Full 64-bit MAINCPU clock (Spec 791.1): a clock past the 32-bit wrap must
    /// import WHOLE, not truncated to the low 32 bits.
    #[test]
    fn maincpu_full_64bit_clock_imported() {
        let clk: u64 = 0x0000_0001_2345_6789; // high dword non-zero
        let mut vsf = vice_header();
        push_vice_module(&mut vsf, "MAINCPU", &vice_maincpu(clk, 0xc000));
        push_vice_module(&mut vsf, "C64MEM", &vice_c64mem(0xc000));
        let mut m = Machine::new();
        let report = load_vsf_report(&mut m, &vsf).expect("load report");
        assert_eq!(report.source, "vice-x64sc");
        assert_eq!(m.c64_core.clk, clk, "full 64-bit clock imported (not truncated)");
        assert_eq!(m.cpu6510.clk, clk, "cpu6510 clock mirror");
        assert_eq!(m.c64_core.reg_pc, 0xc000, "PC restored");
    }

    /// Native-VSF detection by header structure (Spec 791.4): a real VICE header with
    /// NO "SIDEXTENDED" module is still detected + routed to the VICE parser.
    #[test]
    fn native_detection_by_header_not_only_marker() {
        let mut vsf = vice_header();
        push_vice_module(&mut vsf, "MAINCPU", &vice_maincpu(1000, 0xc000));
        push_vice_module(&mut vsf, "C64MEM", &vice_c64mem(0xc000));
        assert!(is_native_vice_vsf(&vsf), "structural detector fires without SIDEXTENDED");
        let mut m = Machine::new();
        let r = load_vsf_report(&mut m, &vsf).expect("load");
        assert_eq!(r.source, "vice-x64sc", "routed to the real-VICE parser");
    }

    /// Fidelity classification (Spec 791.3): a disk-game-shaped VSF carrying a DRIVE8
    /// module we do NOT restore ⇒ `Partial` with DRIVE8 in `absent` — NOT `Faithful`,
    /// NOT the old `errors=[]` footgun.
    #[test]
    fn fidelity_partial_when_drive8_dropped() {
        let mut vsf = vice_header();
        push_vice_module(&mut vsf, "MAINCPU", &vice_maincpu(50_000, 0xc000));
        push_vice_module(&mut vsf, "C64MEM", &vice_c64mem(0xc000));
        // The drive modules a real disk-game VSF carries — present, but slice 1 does
        // not restore them.
        push_vice_module(&mut vsf, "DRIVE8", &[0u8; 8]);
        push_vice_module(&mut vsf, "DRIVECPU0", &[0u8; 8]);
        push_vice_module(&mut vsf, "1541VIA1D0", &[0u8; 8]);

        let mut m = Machine::new();
        let report = load_vsf_report(&mut m, &vsf).expect("load report");

        assert_eq!(report.fidelity, Fidelity::Partial, "critical DRIVE8 dropped ⇒ Partial");
        assert_ne!(report.fidelity, Fidelity::Faithful, "must NOT read as faithful");
        assert!(report.absent.iter().any(|s| s == "DRIVE8"), "DRIVE8 in absent: {:?}", report.absent);
        assert!(report.loaded.iter().any(|s| s == "MAINCPU"), "MAINCPU loaded");
        assert!(report.loaded.iter().any(|s| s == "C64MEM"), "C64MEM loaded");
        // The report REPLACES the errors=[] signal: fidelity is an explicit verdict.
        assert_eq!(report.fidelity.as_str(), "partial");
    }

    /// Fidelity classification (Spec 791.3): a machine-state-core-only VSF (CPU, RAM,
    /// CIA, SID, VIC — no drive, no cart) restores fully ⇒ `Faithful`.
    #[test]
    fn fidelity_faithful_when_core_only() {
        let mut vsf = vice_header();
        push_vice_module(&mut vsf, "MAINCPU", &vice_maincpu(50_000, 0xc000));
        push_vice_module(&mut vsf, "C64MEM", &vice_c64mem(0xc000));
        push_vice_module(&mut vsf, "CIA1", &[0u8; 24]); // >= 20 bytes
        push_vice_module(&mut vsf, "CIA2", &[0u8; 24]);
        // SID: num_sids/sound/engine/model + 32 regs (>= 36 bytes).
        push_vice_module(&mut vsf, "SID", &[0u8; 40]);
        // VIC-IISC: model[0] + 64 regs (>= 65 bytes) — the register head we restore.
        push_vice_module(&mut vsf, "VIC-IISC", &[0u8; 96]);

        let mut m = Machine::new();
        let report = load_vsf_report(&mut m, &vsf).expect("load report");
        assert_eq!(report.fidelity, Fidelity::Faithful, "full core, nothing critical: {report:?}");
        assert!(report.absent.is_empty(), "nothing absent: {:?}", report.absent);
        assert!(report.coarse.is_empty(), "coarse empty in slice 1");
        // The VIC-IISC head is credited to the loaded "VIC-II", not double-listed absent.
        assert!(report.loaded.iter().any(|s| s == "VIC-II"), "VIC head loaded");
    }

    /// Inspection-only: a VSF with no resumable core (no MAINCPU/C64MEM) — only an
    /// inspectable module — classifies `InspectionOnly`, never `Faithful`.
    #[test]
    fn fidelity_inspection_only_without_core() {
        let mut vsf = vice_header();
        push_vice_module(&mut vsf, "SID", &[0u8; 40]);
        // A SIDEXTENDED module forces VICE routing even without a core.
        push_vice_module(&mut vsf, "SIDEXTENDED", &[0u8; 4]);
        let mut m = Machine::new();
        let report = load_vsf_report(&mut m, &vsf).expect("load report");
        assert_eq!(report.fidelity, Fidelity::InspectionOnly, "no CPU/RAM ⇒ inspection-only");
        assert_ne!(report.fidelity, Fidelity::Faithful);
    }

    /// The c64re-own round-trip stays `Faithful` (save_vsf writes the full core), and
    /// `load_vsf_report` classifies it without regressing the compact parser.
    #[test]
    fn c64re_own_roundtrip_reports_faithful() {
        let mut m = Machine::new();
        m.cpu6510.reg_pc = 0x0810;
        m.ram[0x1000] = 0x5a;
        let bytes = save_vsf(&mut m);
        let mut m2 = Machine::new();
        let report = load_vsf_report(&mut m2, &bytes).expect("load report");
        assert_eq!(report.source, "c64re");
        assert_eq!(report.fidelity, Fidelity::Faithful, "c64re-own full-core save ⇒ faithful: {report:?}");
        assert_eq!(m2.ram[0x1000], 0x5a);
    }

    // ── Spec 791 slice 2 — synthetic EasyFlash VSF builders (no real EF `.vsf` needed) ─
    //
    // A real VICE EasyFlash VSF carries a generic "C64CART" module + the EF-specific
    // "CARTEF" (jumper / register_00 / register_02 / IO2-RAM + the two 512K flash
    // arrays) + two "FLASH040EF" command-FSM modules. These builders emit exactly that
    // framing with KNOWN bytes so the loader + fidelity classifier exercise their real
    // paths without a copyrighted game snapshot. Synthetic bytes only (NEUTRALITY).

    /// Byte offset of the ROML flash marker: bank 3, in-bank offset $10 ⇒ (3<<13)|$10.
    const EF_ROML_MARKER_OFF: usize = (3 << 13) | 0x10;
    /// Byte offset of the ROMH flash marker: bank 3, in-bank offset $20 ⇒ (3<<13)|$20.
    const EF_ROMH_MARKER_OFF: usize = (3 << 13) | 0x20;

    /// A minimal generic "C64CART" module body (c64carthooks.c): number_of_carts +
    /// mem_cartridge_type (= 32 EasyFlash) + two export bytes. The loader keys the EF
    /// rebuild on the EF-specific CARTEF module, not this body — this is present so the
    /// fidelity report has a "C64CART" module to bucket.
    fn vice_c64cart_generic() -> Vec<u8> {
        let mut d = Vec::new();
        d.push(1); // number_of_carts
        push_u32_le(&mut d, 32); // mem_cartridge_type = CARTRIDGE_EASYFLASH
        d.push(1); // export.game
        d.push(0); // export.exrom
        d
    }

    /// A "CARTEF" 0.0 module body: jumper / register_00 (bank) / register_02 / RAM[256]
    /// / ROML[512K] / ROMH[512K]. IO2-RAM carries a marker at $10; ROML a marker at
    /// bank-3 offset $10; ROMH a marker at bank-3 offset $20.
    fn vice_cartef(jumper: u8, register_00: u8, register_02: u8) -> Vec<u8> {
        let mut d = Vec::with_capacity(3 + 256 + 2 * 0x80000);
        d.push(jumper);
        d.push(register_00);
        d.push(register_02);
        let mut ram = [0u8; 256];
        ram[0x10] = 0x5a; // IO2-RAM marker
        d.extend_from_slice(&ram);
        let mut roml = vec![0u8; 0x80000];
        roml[EF_ROML_MARKER_OFF] = 0x42; // ROML bank-3 marker
        d.extend_from_slice(&roml);
        let mut romh = vec![0u8; 0x80000];
        romh[EF_ROMH_MARKER_OFF] = 0x99; // ROMH bank-3 marker
        d.extend_from_slice(&romh);
        d
    }

    /// A "FLASH040EF" 2.0 module body: state / base_state / program_byte /
    /// erase_mask[8] / last_read.
    fn vice_flash040ef(state: u8, base_state: u8, program_byte: u8, last_read: u8) -> Vec<u8> {
        let mut d = Vec::new();
        d.push(state);
        d.push(base_state);
        d.push(program_byte);
        d.extend_from_slice(&[0u8; 8]); // erase_mask
        d.push(last_read);
        d
    }

    /// A BankInfo for the read-only-relevant EF read/peek (EasyFlash ignores it).
    fn ef_bankinfo() -> crate::cart::BankInfo {
        crate::cart::BankInfo {
            cpu_port_direction: 0x2f,
            cpu_port_value: 0x37,
            basic_visible: false,
            kernal_visible: false,
            io_visible: true,
            char_visible: false,
            cartridge_attached: true,
            cartridge_exrom: Some(1),
            cartridge_game: Some(1),
            phi1: 0xff,
        }
    }

    /// Build a synthetic EF VSF (header + MAINCPU + C64MEM + C64CART + CARTEF + 2×
    /// FLASH040EF). `jumper`/`bank`/`reg02` seed the CARTEF continuation; both flash
    /// FSMs are left in READ state.
    fn synthetic_ef_vsf(jumper: u8, bank: u8, reg02: u8) -> Vec<u8> {
        let mut vsf = vice_header();
        push_vice_module(&mut vsf, "MAINCPU", &vice_maincpu(1000, 0xc000));
        push_vice_module(&mut vsf, "C64MEM", &vice_c64mem(0xc000));
        push_vice_module(&mut vsf, "C64CART", &vice_c64cart_generic());
        push_vice_module(&mut vsf, "CARTEF", &vice_cartef(jumper, bank, reg02));
        push_vice_module(&mut vsf, "FLASH040EF", &vice_flash040ef(0, 0, 0, 0)); // lo
        push_vice_module(&mut vsf, "FLASH040EF", &vice_flash040ef(0, 0, 0, 0)); // hi
        vsf
    }

    /// Spec 791 slice 2 (primary EF test): a synthetic EF VSF reconstructs the
    /// EasyFlashMapper — bank / register_02 / jumper / IO2-RAM / the flash arrays — and
    /// the live mapper reads back those exact values (bank-select → the known flash
    /// byte; lines per the restored mode).
    #[test]
    fn ef_cart_reconstructed_from_vsf() {
        let jumper = 1u8;
        let bank = 3u8;
        let reg02 = 0x80u8; // LED on, mode bits 0 ⇒ jumper drives Ultimax(off)/Off
        let vsf = synthetic_ef_vsf(jumper, bank, reg02);

        let mut m = Machine::new();
        let report = load_vsf_report(&mut m, &vsf).expect("load report");
        assert_eq!(report.source, "vice-x64sc");

        {
            let cart = m.cartridge.as_ref().expect("EF cart attached");
            assert_eq!(cart.mapper_type(), crate::cart::MapperType::EasyFlash);
            let st = cart.get_state();
            assert_eq!(st.current_bank, bank as u16, "bank (register_00) restored");
            assert_eq!(st.control_register, Some(reg02), "register_02 restored");
            let f = st.flash.expect("flash continuation");
            assert_eq!(f.easyflash_jumper, jumper, "jumper restored");
            assert_eq!(f.easyflash_ram[0x10], 0x5a, "IO2 RAM restored");
            // get_lines per the restored mode: reg02 mode-bits 0, jumper on ⇒
            // memconfig[(1<<3)|0]=2 = Off ⇒ {exrom:1, game:1}.
            let lines = cart.get_lines();
            assert_eq!((lines.exrom, lines.game), (1, 1), "Off-mode lines (jumper on)");
        }
        // Bank-select → known flash byte: current_bank=3 maps $8010→ROML (3<<13)|$10 and
        // $A020→ROMH (3<<13)|$20 in the restored flash arrays.
        let bi = ef_bankinfo();
        let cart = m.cartridge.as_ref().unwrap();
        assert_eq!(cart.peek(0x8010, &bi), Some(0x42), "ROML bank-3 flash byte");
        assert_eq!(cart.peek(0xa020, &bi), Some(0x99), "ROMH bank-3 flash byte");
    }

    /// Spec 791 slice 2 (fidelity): a synthetic EF VSF moves `C64CART` from the slice-1
    /// `absent` footgun into `loaded`; a still-unrestored drive keeps the verdict honest
    /// (`Partial`, not `Faithful`).
    #[test]
    fn fidelity_ef_c64cart_loaded_not_absent() {
        let mut vsf = synthetic_ef_vsf(0, 0, 0);
        // A drive module present-but-unrestored ⇒ the remaining gap keeps it Partial.
        push_vice_module(&mut vsf, "DRIVE8", &[0u8; 8]);

        let mut m = Machine::new();
        let report = load_vsf_report(&mut m, &vsf).expect("load report");
        assert!(report.loaded.iter().any(|s| s == "C64CART"), "C64CART loaded: {report:?}");
        assert!(!report.absent.iter().any(|s| s == "C64CART"), "C64CART not absent: {:?}", report.absent);
        assert!(report.loaded.iter().any(|s| s == "CARTEF"), "CARTEF loaded");
        assert!(report.loaded.iter().any(|s| s == "FLASH040EF"), "FLASH040EF loaded");
        assert_eq!(report.fidelity, Fidelity::Partial, "drive gap keeps it Partial: {report:?}");
        assert!(report.absent.iter().any(|s| s == "DRIVE8"), "drive still absent");
    }

    /// Spec 791 slice 2 (`.c64re` embed): the RuntimeCheckpoint captured from a
    /// VSF-reconstructed EF machine embeds the cart `.crt` bytes + the writable flash
    /// image, so a fresh restore (= `undump` / `sandbox --seed`) re-attaches the
    /// EasyFlash cart with the restored flash bytes, byte-exact.
    #[test]
    fn c64re_embeds_ef_cart_and_flash() {
        let vsf = synthetic_ef_vsf(1, 3, 0x80);
        let mut m = Machine::new();
        load_vsf_report(&mut m, &vsf).expect("load");

        // Capture the cart blobs exactly as convert_cmd / the daemon do.
        let clk = m.c64_core.clk;
        let cart_bytes = m
            .cartridge_image
            .as_ref()
            .map(|i| i.raw_bytes.clone())
            .expect("cart .crt bytes present");
        let cart_flash = m
            .cartridge
            .as_mut()
            .and_then(|c| c.writable_image(clk))
            .expect("cart writable flash present");
        assert_eq!(cart_flash.len(), 2 * 0x80000, "lo+hi flash = 1 MiB");
        assert_eq!(cart_flash[EF_ROML_MARKER_OFF], 0x42, "ROML marker in captured flash");
        assert_eq!(cart_flash[0x80000 + EF_ROMH_MARKER_OFF], 0x99, "ROMH marker in captured flash");

        let cp = crate::c64re_snapshot::capture_runtime_checkpoint(
            &m,
            "",
            "",
            None,
            None,
            Some(&cart_bytes),
            Some(&cart_flash),
        );

        // Re-load into a fresh machine (= the undump / sandbox --seed path).
        let mut m2 = Machine::new();
        crate::c64re_snapshot::restore_runtime_checkpoint(&mut m2, &cp).expect("restore .c64re");

        let cart2 = m2.cartridge.as_mut().expect("EF cart re-attached from .c64re");
        assert_eq!(cart2.mapper_type(), crate::cart::MapperType::EasyFlash);
        let flash2 = cart2.writable_image(0).expect("writable image after restore");
        assert_eq!(flash2, cart_flash, "flash bytes round-trip through .c64re byte-exact");
    }
}
