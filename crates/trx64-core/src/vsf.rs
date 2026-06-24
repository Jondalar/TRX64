//! vsf.rs — VICE Snapshot Format (VSF) save/load for Machine state.
//!
//! Binary layout reference:
//!   C64ReverseEngineeringMCP/src/runtime/headless/vsf/vsf-format.ts
//!   C64ReverseEngineeringMCP/src/runtime/headless/vsf/module-mapping.ts
//!   C64ReverseEngineeringMCP/src/runtime/headless/vsf/session-vsf.ts
//!
//! Module byte counts:
//!   MAINCPU   11 bytes
//!   C64MEM    65550 bytes
//!   CIA1      48 bytes
//!   CIA2      48 bytes
//!   SID       32 bytes
//!   DRIVECPU  0 bytes
//!   IECBUS    6 bytes
//!   VIC-II    108 bytes
//!   KEYBOARD  6 bytes

use crate::cia::CIA_ICR;
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

// ── Public types ──────────────────────────────────────────────────────────────

/// Result of a VSF load operation.
#[derive(Debug)]
pub struct VsfLoadResult {
    pub loaded_modules: Vec<String>,
    pub ignored_modules: Vec<String>,
    pub errors: Vec<(String, String)>,
    pub source: &'static str,
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

/// DRIVECPU module (0 bytes — stub).
fn ser_drivecpu() -> Vec<u8> {
    Vec::new()
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
pub fn save_vsf(machine: &Machine) -> Vec<u8> {
    let mut buf = Vec::with_capacity(70_000);

    // VSF file header.
    buf.extend_from_slice(VSF_MAGIC);
    buf.push(VSF_MAJOR);
    buf.push(VSF_MINOR);
    buf.extend_from_slice(VSF_MACHINE);

    // Module order MUST match TS: MAINCPU, C64MEM, CIA1, CIA2, SID, DRIVECPU,
    // IECBUS, VIC-II, KEYBOARD.
    write_module(&mut buf, b"MAINCPU", &ser_maincpu(machine));
    write_module(&mut buf, b"C64MEM", &ser_c64mem(machine));

    let clk = machine.clk;
    write_module(&mut buf, b"CIA1", &ser_cia(&machine.cia1, clk));
    write_module(&mut buf, b"CIA2", &ser_cia(&machine.cia2, clk));

    write_module(&mut buf, b"SID", &ser_sid(machine));
    write_module(&mut buf, b"DRIVECPU", &ser_drivecpu());
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

// ── Public load function ──────────────────────────────────────────────────────

/// Restore machine state from VSF bytes.
///
/// Auto-detects VICE x64sc snapshots (contains "SIDEXTENDED" ASCII) and refuses
/// them gracefully. c64re snapshots are parsed module-by-module.
pub fn load_vsf(machine: &mut Machine, data: &[u8]) -> Result<VsfLoadResult, String> {
    // VICE detection: if bytes contain "SIDEXTENDED" it's a VICE file.
    if data.windows(VICE_MARKER.len()).any(|w| w == VICE_MARKER) {
        return Err("vice-x64sc snapshots not supported in load path".to_string());
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
            "DRIVECPU" => Ok(()), // empty stub
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

        let bytes = save_vsf(&m);
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

        let bytes = save_vsf(&m);
        let mut m2 = Machine::new();
        let result = load_vsf(&mut m2, &bytes).expect("load failed");
        assert!(result.errors.is_empty());

        assert_eq!(m2.ram[0x1000], 0xDE);
        assert_eq!(m2.ram[0x2000], 0xAD);
        assert_eq!(m2.port_dir, 0x2F);
        assert_eq!(m2.port_data, 0x37);
    }

    #[test]
    fn vice_marker_rejected() {
        // Build a fake VSF that contains "SIDEXTENDED" somewhere.
        let mut fake = Vec::new();
        fake.extend_from_slice(VSF_MAGIC);
        fake.push(VSF_MAJOR);
        fake.push(VSF_MINOR);
        fake.extend_from_slice(VSF_MACHINE);
        fake.extend_from_slice(b"SIDEXTENDED\0\x01\x00\x00\x00\x00\x00");
        let mut m = Machine::new();
        let r = load_vsf(&mut m, &fake);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("vice-x64sc"));
    }

    #[test]
    fn module_byte_counts() {
        let m = Machine::new();
        assert_eq!(ser_maincpu(&m).len(), 11, "MAINCPU");
        assert_eq!(ser_c64mem(&m).len(), 65550, "C64MEM");
        assert_eq!(ser_cia(&m.cia1, 0).len(), 48, "CIA");
        assert_eq!(ser_sid(&m).len(), 32, "SID");
        assert_eq!(ser_drivecpu().len(), 0, "DRIVECPU");
        assert_eq!(ser_iecbus(&m).len(), 6, "IECBUS");
        assert_eq!(ser_vicii(&m).len(), 108, "VIC-II");
        assert_eq!(ser_keyboard(&m).len(), 6, "KEYBOARD");
    }
}
