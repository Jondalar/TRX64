//! c64re_snapshot.rs — the structured `.c64re` RuntimeCheckpoint (ADR-077).
//!
//! 1:1 PORT of the c64re TS
//!   C64ReverseEngineeringMCP/src/runtime/headless/kernel/runtime-checkpoint.ts
//! + the kernel `capture()`/`restore()` field mapping
//!   (headless-machine-kernel.ts:946-1084).
//!
//! Every serde struct here serializes to the EXACT JSON shape (field names,
//! nesting, types) the c64re kernel writes, so a live c64re daemon can
//! `snapshot/undump` a TRX64 `.c64re` dump and vice-versa. The serializers are
//! ADDITIVE READS of the existing TRX64 chip state (the VSF/ADR-076 state) —
//! they never touch the cycle/opcode logic.
//!
//! Parity notes vs the c64re shapes (where TRX64's DISTILLED chips lack a
//! VICE-internal field, the SAME derived placeholder the existing VSF `ser_cia`
//! uses is emitted — the byte-exact gates are the guard; both runtimes resume
//! from the register file + RAM + CPU + timer state which IS captured):
//!   - cpu:           1:1 (pc/a/x/y/sp/flags/cycles + maincpu_ba_low_flags).
//!   - ram/cpuPort*:  1:1.
//!   - cia1/cia2:     register file + timers (state/latch/cnt/clk) + irqflags +
//!                    ta/tb alarm clk are 1:1; the IFR delay-line pipeline, the
//!                    SDR submodule, and the extended TOD fields are DISTILLED
//!                    (emitted as the VICE-default placeholders — see CiaSnapshot).
//!   - sid:           regs[32] + voice state 1:1 (gateflip=0 at a boundary).
//!   - iec:           1:1.
//!   - cpuIntStatus:  TRX64's [u32;4] per-source model mapped to the c64re
//!                    pendingInt/intNames arrays (canonical source names).
//!   - alarmsMaincpu: [] — TRX64's maincpu is NOT alarm-driven (distilled
//!                    IntStatus, not the VICE alarm context). The drive's VIA
//!                    alarms ride the drive blob, exactly as runtime-checkpoint.ts
//!                    documents.

use serde::{Deserialize, Serialize};

use crate::cia::{
    Cia, CIA_CRA, CIA_CRB, CIA_ICR, CIA_SDR, CIA_TAH, CIA_TAL, CIA_TBH, CIA_TBL, CIA_TOD_HR,
    CIA_TOD_MIN, CIA_TOD_SEC, CIA_TOD_TEN, CLOCK_NEVER,
};
use crate::native_snapshot::{ta_u8, ta_u8_decode};
use crate::Machine;

/// runtime-checkpoint.ts:27 — `RUNTIME_CHECKPOINT_SCHEMA_VERSION = 1`.
pub const RUNTIME_CHECKPOINT_SCHEMA_VERSION: i64 = 1;

/// CIA model 0 (CIA_MODEL_6526) — cia6526-vice.ts:169.
const CIA_MODEL_6526: i64 = 0;

// ── cpu (runtime-checkpoint.ts:29-36) ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuSnapshot {
    pub pc: i64,
    pub a: i64,
    pub x: i64,
    pub y: i64,
    pub sp: i64,
    pub flags: i64,
    pub cycles: i64,
    #[serde(rename = "maincpu_ba_low_flags", skip_serializing_if = "Option::is_none")]
    pub maincpu_ba_low_flags: Option<i64>,
    #[serde(rename = "soLine", skip_serializing_if = "Option::is_none")]
    pub so_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jammed: Option<bool>,
}

// ── iec (runtime-checkpoint.ts:53-61) ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IecSnapshot {
    pub cpu_bus: i64,
    pub cpu_port: i64,
    pub drv_port: i64,
    pub iec_old_atn: i64,
    pub drv_bus: Vec<i64>,  // [16]
    pub drv_data: Vec<i64>, // [16]
}

// ── cpuIntStatus (runtime-checkpoint.ts:39-51) ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntStatusSnapshot {
    #[serde(rename = "pendingInt")]
    pub pending_int: Vec<i64>,
    #[serde(rename = "intNames")]
    pub int_names: Vec<String>,
    pub nirq: i64,
    pub nnmi: i64,
    #[serde(rename = "irqClk")]
    pub irq_clk: i64,
    #[serde(rename = "nmiClk")]
    pub nmi_clk: i64,
    #[serde(rename = "irqDelayCycles")]
    pub irq_delay_cycles: i64,
    #[serde(rename = "nmiDelayCycles")]
    pub nmi_delay_cycles: i64,
    #[serde(rename = "irqPendingClk")]
    pub irq_pending_clk: i64,
    #[serde(rename = "globalPendingInt")]
    pub global_pending_int: i64,
    #[serde(rename = "lastStolenCyclesClk")]
    pub last_stolen_cycles_clk: i64,
}

// ── cia (cia6526-vice.ts:243-290 — Cia6526ViceSnapshot) ────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiaSnapshot {
    pub v: i64, // = 2
    pub c_cia: Vec<i64>, // [16]
    pub irqflags: i64,
    pub ack_irqflags: i64,
    pub new_irqflags: i64,
    pub irq_enabled: i64,
    pub rdi: i64,
    pub ifr_clock: i64,
    pub ifr_delay: i64,
    pub tat: i64,
    pub tbt: i64,
    pub ta_state: i64,
    pub ta_latch: i64,
    pub ta_cnt: i64,
    pub ta_clk: i64,
    pub tb_state: i64,
    pub tb_latch: i64,
    pub tb_cnt: i64,
    pub tb_clk: i64,
    pub sr_bits: i64,
    pub sdr_valid: i64,
    pub sdr_force_finish: i64,
    pub shifter: i64,
    pub sdr_delay: i64,
    pub sp_in_state: i64,
    pub cnt_in_state: i64,
    pub cnt_out_state: i64,
    pub todalarm: Vec<i64>, // [4]
    pub todlatch: Vec<i64>, // [4]
    pub todlatched: i64,
    pub todstopped: i64,
    pub todticks: i64,
    pub todclk: i64,
    pub todtickcounter: i64,
    pub power_tickcounter: i64,
    pub power_ticks: i64,
    pub old_pa: i64,
    pub old_pb: i64,
    pub read_clk: i64,
    pub read_offset: i64,
    pub last_read: i64,
    pub model: i64,
}

// ── sid (sid.ts:167-177 — SidSnapshot) ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidVoiceSnapshot {
    pub f: i64,
    pub fs: i64,
    pub pw: i64,
    pub noise: i64,
    pub wt_select: i64,
    pub attack: i64,
    pub decay: i64,
    pub sustain: i64,
    pub release: i64,
    pub sync: i64,
    pub adsrm: i64,
    pub adsr_value: i64,
    pub cycle_accum: i64,
    pub gateflip: i64,
    pub prev_gate: i64,
    pub rv: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidSnapshot {
    pub v: i64, // = 2
    pub regs: Vec<i64>, // [32]
    pub voices: Vec<SidVoiceSnapshot>, // [3]
}

// ── vic (vicii-snapshot.ts:32-83 — LiteralVicSnapshot) ─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VicSpriteSnapshot {
    pub data: i64,
    pub mc: i64,
    pub mcbase: i64,
    pub pointer: i64,
    pub exp_flop: i64,
    pub x: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VicLightPenSnapshot {
    pub state: i64,
    pub triggered: i64,
    pub x: i64,
    pub y: i64,
    pub x_extra_bits: i64,
    pub trigger_cycle: i64,
}

/// vicii-draw-cycle.ts:100-121 — `DrawCycleSnapshot`. Plain-number arrays stay as
/// JSON arrays; the `Uint8Array`/`Uint32Array` fields ride as `{ $ta }` nodes
/// (matching how c64re's `encodeValue` tags them in the container).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrawCycleSnapshot {
    pub gbuf_pipe0_reg: i64,
    pub cbuf_pipe0_reg: i64,
    pub vbuf_pipe0_reg: i64,
    pub gbuf_pipe1_reg: i64,
    pub cbuf_pipe1_reg: i64,
    pub vbuf_pipe1_reg: i64,
    pub xscroll_pipe: i64,
    pub vmode11_pipe: i64,
    pub vmode16_pipe: i64,
    pub vmode16_pipe2: i64,
    pub gbuf_reg: i64,
    pub gbuf_mc_flop: i64,
    pub gbuf_pixel_reg: i64,
    pub cbuf_reg: i64,
    pub vbuf_reg: i64,
    pub dmli: i64,
    pub sprite_x_pipe: Vec<i64>, // [8] plain
    pub sprite_pri_bits: i64,
    pub sprite_mc_bits: i64,
    pub sprite_expx_bits: i64,
    pub sprite_pending_bits: i64,
    pub sprite_active_bits: i64,
    pub sprite_halt_bits: i64,
    pub sbuf_reg: serde_json::Value,       // $ta Uint32Array[8]
    pub sbuf_pixel_reg: serde_json::Value, // $ta Uint8Array[8]
    pub sbuf_expx_flops: i64,
    pub sbuf_mc_flops: i64,
    pub border_state: i64,
    pub render_buffer: serde_json::Value, // $ta Uint8Array[8]
    pub pri_buffer: serde_json::Value,    // $ta Uint8Array[8]
    pub pixel_buffer: serde_json::Value,  // $ta Uint8Array[8]
    pub cregs: serde_json::Value,         // $ta Uint8Array[0x2f]
    pub last_color_reg: i64,
    pub last_color_value: i64,
    pub cycle_flags_pipe: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VicSnapshot {
    pub model: i64,
    pub regs: Vec<i64>, // [0x40]
    pub raster_cycle: i64,
    pub cycle_flags: i64,
    pub raster_line: i64,
    pub start_of_frame: i64,
    pub irq_status: i64,
    pub raster_irq_line: i64,
    pub raster_irq_triggered: i64,
    pub vbuf: Vec<i64>, // [40]
    pub cbuf: Vec<i64>, // [40]
    pub gbuf: i64,
    pub dbuf_offset: i64,
    pub dbuf: Vec<i64>, // [520]
    pub ysmooth: i64,
    pub allow_bad_lines: i64,
    pub sprite_sprite_collisions: i64,
    pub sprite_background_collisions: i64,
    pub clear_collisions: i64,
    pub idle_state: i64,
    pub vcbase: i64,
    pub vc: i64,
    pub rc: i64,
    pub vmli: i64,
    pub bad_line: i64,
    pub light_pen: VicLightPenSnapshot,
    pub reg11_delay: i64,
    pub prefetch_cycles: i64,
    pub sprite_display_bits: i64,
    pub sprite_dma: i64,
    pub last_color_reg: i64,
    pub last_color_value: i64,
    pub last_read_phi1: i64,
    pub last_bus_phi2: i64,
    pub vborder: i64,
    pub set_vborder: i64,
    pub main_border: i64,
    pub refresh_counter: i64,
    pub color_ram: Vec<i64>, // [0x400]
    pub sprite: Vec<VicSpriteSnapshot>, // [8]
    #[serde(rename = "drawCycle")]
    pub draw_cycle: DrawCycleSnapshot,
}

// ── vicPresentation (runtime-checkpoint.ts:79-85) ──────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VicPresentationSnapshot {
    /// `{ $ta }` Uint8Array | null — the mid-frame accumulator framebuffer.
    #[serde(rename = "literalPortFb")]
    pub literal_port_fb: serde_json::Value,
    /// `{ $ta }` Uint8Array | null — the immediately-visible freeze image.
    #[serde(rename = "literalPortFbStable")]
    pub literal_port_fb_stable: serde_json::Value,
    #[serde(rename = "litLastRasterLine")]
    pub lit_last_raster_line: i64,
    #[serde(rename = "lastLitBaLow")]
    pub last_lit_ba_low: i64,
    #[serde(rename = "litStableFrameCount")]
    pub lit_stable_frame_count: i64,
}

// ── helpers ────────────────────────────────────────────────────────────────────

/// 32-bit alarm-clk widening, identical to vsf.rs `widen_alarmclk`: a stopped
/// timer's cached clk is CLOCK_NEVER (u64::MAX). c64re stores the FULL monotonic
/// value (no u32 wrap, Spec 743). We pass through CLOCK_NEVER → JS number-safe
/// sentinel (Number.MAX_SAFE_INTEGER) so the c64re side reads it as "never".
const JS_MAX_SAFE_INT: i64 = 9_007_199_254_740_991; // Number.MAX_SAFE_INTEGER

fn alarmclk_to_json(v: u64) -> i64 {
    if v == CLOCK_NEVER {
        JS_MAX_SAFE_INT
    } else {
        // Master clocks stay well below 2^53 within a session; cast is exact.
        v as i64
    }
}

fn alarmclk_from_json(v: i64) -> u64 {
    if v == JS_MAX_SAFE_INT || v < 0 {
        CLOCK_NEVER
    } else {
        v as u64
    }
}

// ── per-chip capture ───────────────────────────────────────────────────────────

/// Read a TRX64 `Cia` into the c64re `Cia6526ViceSnapshot` shape.
///
/// TRX64's `Cia` is DISTILLED relative to VICE's ciacore. The fields it carries
/// (register file, timer state/latch/cnt/clk, irqflags, ta/tb alarm clk, the
/// latched TOD) map 1:1; the VICE-internal pipeline fields it does NOT carry are
/// emitted as the documented VICE-default placeholders (the SAME values the
/// existing `vsf.rs::ser_cia` writes, where the byte-exact gates already pass):
///   - ack_irqflags/new_irqflags/ifr_delay = 0 (no IFR delay-line in TRX64).
///   - rdi/ifr_clock/read_clk = the chip clk.
///   - SDR submodule (sr_bits/shifter/sdr_*/*_in_state) = 0.
///   - old_pa/old_pb = 0xff (VICE bug #1143, cia6526-vice.ts:416-418).
///   - tod power/tick counters = 0; todalarm = 0s.
pub fn capture_cia(cia: &Cia) -> CiaSnapshot {
    let clk = cia.clk as i64;
    CiaSnapshot {
        v: 2,
        c_cia: cia.regs.iter().map(|&b| b as i64).collect(),
        irqflags: cia.irqflags as i64,
        ack_irqflags: 0,
        new_irqflags: 0,
        irq_enabled: cia.regs[CIA_ICR] as i64,
        rdi: clk,
        ifr_clock: clk,
        ifr_delay: 0,
        tat: cia.ta.is_running() as i64,
        tbt: cia.tb.is_running() as i64,
        ta_state: cia.ta.state as i64,
        ta_latch: cia.ta.latch as i64,
        ta_cnt: cia.ta.cnt as i64,
        ta_clk: cia.ta.clk as i64,
        tb_state: cia.tb.state as i64,
        tb_latch: cia.tb.latch as i64,
        tb_cnt: cia.tb.cnt as i64,
        tb_clk: cia.tb.clk as i64,
        sr_bits: 0,
        sdr_valid: 0,
        sdr_force_finish: 0,
        shifter: 0,
        sdr_delay: 0,
        sp_in_state: 0,
        cnt_in_state: 0,
        cnt_out_state: 0,
        todalarm: vec![0, 0, 0, 0],
        todlatch: cia.tod_latch.iter().map(|&b| b as i64).collect(),
        todlatched: cia.tod_latched as i64,
        todstopped: 0,
        todticks: 0,
        todclk: clk,
        todtickcounter: 0,
        power_tickcounter: cia.tod_prescaler as i64,
        power_ticks: 0,
        old_pa: 0xff,
        old_pb: 0xff,
        read_clk: clk,
        read_offset: 0,
        last_read: 0,
        model: CIA_MODEL_6526,
        // ta/tb alarm clk: TRX64 caches them on the Cia (ta_alarmclk/tb_alarmclk);
        // the c64re snapshot folds them into the Ciat alarm via ta_clk/tb_clk on
        // restore (the chip re-derives the alarm on the first register access). We
        // do not emit a separate field (the c64re shape has none — it keeps the
        // alarm clk private + re-arms via alarmsMaincpu, which TRX64 emits []).
    }
}

/// Restore a TRX64 `Cia` from the c64re `Cia6526ViceSnapshot` shape. Mirrors the
/// VSF `load_cia` reconstruction (register file → timer latches/clk → alarm clk).
pub fn restore_cia(cia: &mut Cia, s: &CiaSnapshot) {
    for i in 0..16 {
        cia.regs[i] = s.c_cia.get(i).copied().unwrap_or(0) as u8;
    }
    cia.irqflags = s.irqflags as u8;
    cia.regs[CIA_ICR] = s.irq_enabled as u8;
    cia.ta.state = s.ta_state as u16;
    cia.ta.latch = s.ta_latch as u16;
    cia.ta.cnt = s.ta_cnt as u16;
    cia.ta.clk = s.ta_clk as u64;
    cia.tb.state = s.tb_state as u16;
    cia.tb.latch = s.tb_latch as u16;
    cia.tb.cnt = s.tb_cnt as u16;
    cia.tb.clk = s.tb_clk as u64;
    cia.clk = s.read_clk as u64;
    for i in 0..4 {
        cia.tod_latch[i] = s.todlatch.get(i).copied().unwrap_or(0) as u8;
    }
    cia.tod_latched = s.todlatched != 0;
    cia.tod_prescaler = s.power_tickcounter as u32;
    // Re-derive the cached alarm clk from the restored timer (stopped → NEVER).
    cia.ta_alarmclk = if cia.ta.is_running() { cia.ta.clk } else { CLOCK_NEVER };
    cia.tb_alarmclk = if cia.tb.is_running() { cia.tb.clk } else { CLOCK_NEVER };
}

/// Read TRX64's SID (`sid` voice state + `sid_regs`) into the c64re `SidSnapshot`.
pub fn capture_sid(m: &Machine) -> SidSnapshot {
    let regs: Vec<i64> = m.sid_regs.iter().map(|&b| b as i64).collect();
    let mut voices = Vec::with_capacity(3);
    for i in 0..3 {
        let (f, fs, pw, noise, wt, a, d, su, r, sy, am, av, ca, gf, pg, rv) = m.sid.c64re_voice(i);
        voices.push(SidVoiceSnapshot {
            f: f as i64, fs: fs as i64, pw: pw as i64, noise: noise as i64,
            wt_select: wt as i64, attack: a as i64, decay: d as i64,
            sustain: su as i64, release: r as i64, sync: sy as i64,
            adsrm: am as i64, adsr_value: av as i64, cycle_accum: ca as i64,
            gateflip: gf as i64, prev_gate: pg as i64, rv: rv as i64,
        });
    }
    SidSnapshot { v: 2, regs, voices }
}

/// Restore TRX64's SID from the c64re `SidSnapshot`.
pub fn restore_sid(m: &mut Machine, s: &SidSnapshot) {
    for i in 0..32 {
        m.sid_regs[i] = s.regs.get(i).copied().unwrap_or(0) as u8;
    }
    for (i, vc) in s.voices.iter().enumerate().take(3) {
        m.sid.c64re_set_voice(
            i,
            vc.f as u32, vc.fs as u32, vc.pw as u32, vc.noise as u8, vc.wt_select as u8,
            vc.attack as u8, vc.decay as u8, vc.sustain as u8, vc.release as u8, vc.sync as u8,
            vc.adsrm as u8, vc.adsr_value as u8, vc.cycle_accum as u32,
            vc.gateflip as u8, vc.prev_gate as u8, vc.rv as u32,
        );
    }
}

/// Read TRX64's CPU (the PRODUCTION verbatim `c64_core`) into the c64re `cpu`
/// shape. `maincpu_ba_low_flags` mirrors VICE's continuation field (the VIC
/// BA-low state); TRX64 holds it on `vic.ba_low_flag`.
pub fn capture_cpu(m: &Machine) -> CpuSnapshot {
    let c = &m.c64_core;
    CpuSnapshot {
        pc: c.reg_pc as i64,
        a: c.reg_a as i64,
        x: c.reg_x as i64,
        y: c.reg_y as i64,
        sp: c.reg_sp as i64,
        flags: c.status() as i64,
        cycles: c.clk as i64,
        maincpu_ba_low_flags: Some(m.vic.ba_low_flag as i64),
        so_line: None,
        jammed: None,
    }
}

/// Restore TRX64's CPU from the c64re `cpu` shape. Seeds BOTH the production
/// `c64_core` and the legacy `cpu6510` mirror (matching vsf.rs `load_maincpu`).
pub fn restore_cpu(m: &mut Machine, s: &CpuSnapshot) {
    let pc = s.pc as u16;
    let a = s.a as u8;
    let x = s.x as u8;
    let y = s.y as u8;
    let sp = s.sp as u8;
    let flags = s.flags as u8;
    let clk = s.cycles as u64;

    let core = &mut m.c64_core;
    core.reg_pc = pc;
    core.reg_a = a;
    core.reg_x = x;
    core.reg_y = y;
    core.reg_sp = sp;
    core.set_status_composite(flags);
    core.clk = clk;

    let cpu = &mut m.cpu6510;
    cpu.reg_pc = pc;
    cpu.reg_a = a;
    cpu.reg_x = x;
    cpu.reg_y = y;
    cpu.reg_sp = sp;
    cpu.reg_p = flags & !0xa2;
    cpu.flag_n = flags & 0x80;
    cpu.flag_z = if flags & 0x02 != 0 { 0 } else { 1 };
    cpu.clk = clk;

    if let Some(ba) = s.maincpu_ba_low_flags {
        m.vic.ba_low_flag = ba != 0;
    }
}

/// Read TRX64's IEC bus core into the c64re `iec` shape (1:1).
pub fn capture_iec(m: &Machine) -> IecSnapshot {
    let b = &m.iec.iecbus;
    IecSnapshot {
        cpu_bus: b.cpu_bus as i64,
        cpu_port: b.cpu_port as i64,
        drv_port: b.drv_port as i64,
        iec_old_atn: m.iec.iec_old_atn as i64,
        drv_bus: b.drv_bus.iter().map(|&x| x as i64).collect(),
        drv_data: b.drv_data.iter().map(|&x| x as i64).collect(),
    }
}

/// Restore TRX64's IEC bus core from the c64re `iec` shape.
pub fn restore_iec(m: &mut Machine, s: &IecSnapshot) {
    let b = &mut m.iec.iecbus;
    b.cpu_bus = s.cpu_bus as u8;
    b.cpu_port = s.cpu_port as u8;
    b.drv_port = s.drv_port as u8;
    m.iec.iec_old_atn = s.iec_old_atn as u8;
    for (i, &v) in s.drv_bus.iter().enumerate().take(b.drv_bus.len()) {
        b.drv_bus[i] = v as u8;
    }
    for (i, &v) in s.drv_data.iter().enumerate().take(b.drv_data.len()) {
        b.drv_data[i] = v as u8;
    }
}

/// Canonical c64re maincpu interrupt-source names, in TRX64's `pending_int[]`
/// index order (c64_6510core.rs INT_SRC_*: VIC=0, CIA1=1, CIA2=2, RESTORE=3).
/// The c64re side restores `intNames`/`pendingInt` wholesale (kernel restore
/// line 1037-1038), so emitting a fixed-order array round-trips both ways.
const INT_SOURCE_NAMES: [&str; 4] = ["vic-irq", "CIA1", "CIA2", "restore-nmi"];

/// Read TRX64's `c64_int` (distilled IntStatus) into the c64re `cpuIntStatus`
/// shape. The c64re model is a name-indexed list; TRX64 is a fixed [u32;4] per
/// source — we emit the four canonical-named entries in source order.
pub fn capture_int_status(m: &Machine) -> IntStatusSnapshot {
    let cs = &m.c64_int;
    IntStatusSnapshot {
        pending_int: cs.pending_int.iter().map(|&p| p as i64).collect(),
        int_names: INT_SOURCE_NAMES.iter().map(|s| s.to_string()).collect(),
        nirq: cs.nirq as i64,
        nnmi: cs.nnmi as i64,
        irq_clk: alarmclk_to_json(cs.irq_clk),
        nmi_clk: alarmclk_to_json(cs.nmi_clk),
        irq_delay_cycles: cs.irq_delay_cycles as i64,
        nmi_delay_cycles: cs.nmi_delay_cycles as i64,
        irq_pending_clk: alarmclk_to_json(cs.irq_pending_clk),
        global_pending_int: cs.global_pending_int as i64,
        last_stolen_cycles_clk: cs.last_stolen_cycles_clk as i64,
    }
}

/// Restore TRX64's `c64_int` from the c64re `cpuIntStatus`. Maps the c64re
/// name-indexed `pendingInt` back into TRX64's fixed [u32;4] by NAME (so a
/// c64re dump whose source order differs still lands in the right slot).
pub fn restore_int_status(m: &mut Machine, s: &IntStatusSnapshot) {
    let cs = &mut m.c64_int;
    // Map by name when names are present (cross-runtime); else positional.
    let mut pend = [0u32; 4];
    if s.int_names.len() == s.pending_int.len() && !s.int_names.is_empty() {
        for (name, &p) in s.int_names.iter().zip(s.pending_int.iter()) {
            if let Some(idx) = INT_SOURCE_NAMES.iter().position(|n| n == name) {
                pend[idx] = p as u32;
            }
        }
    } else {
        for (i, &p) in s.pending_int.iter().enumerate().take(4) {
            pend[i] = p as u32;
        }
    }
    cs.pending_int = pend;
    cs.nirq = s.nirq as i32;
    cs.nnmi = s.nnmi as i32;
    cs.irq_clk = alarmclk_from_json(s.irq_clk);
    cs.nmi_clk = alarmclk_from_json(s.nmi_clk);
    cs.irq_delay_cycles = s.irq_delay_cycles as u64;
    cs.nmi_delay_cycles = s.nmi_delay_cycles as u64;
    cs.irq_pending_clk = alarmclk_from_json(s.irq_pending_clk);
    cs.global_pending_int = s.global_pending_int as u32;
    cs.last_stolen_cycles_clk = s.last_stolen_cycles_clk as u64;
}

// ── VIC capture/restore (vicii-snapshot.ts:93-216) ─────────────────────────────
//
// TRX64's `VicII` carries the FULL literal-port field set (it is a verbatim
// viciisc port): the public chip fields + the `pub(crate)` draw-cycle pipeline
// statics. The one structural difference: TRX64's `dbuf` is a FULL-FRAME 520×312
// accumulator (the rendered color-index frame), while c64re's `LiteralVicSnapshot.
// dbuf` is the single 520-byte CURRENT draw line. We map the current line
// (`dbuf[dbuf_line*520 .. +520]`) to c64re's `dbuf[520]`, and the full frames ride
// the `vicPresentation` framebuffers (literalPortFb = dbuf, literalPortFbStable =
// displayed) — exactly the c64re split (the draw-line vs the presentation FB).

use crate::native_snapshot::{ta_u32, ta_u32_decode};
use crate::render::FB_W;

/// Read TRX64's `m.vic` + color RAM into the c64re `LiteralVicSnapshot` shape.
pub fn capture_vic(m: &Machine) -> VicSnapshot {
    let v = &m.vic;
    let color_ram = read_color_ram(m);

    // Current draw line out of the full-frame accumulator (c64re's 520-byte dbuf).
    let line = v.dbuf_line.min(crate::render::FB_H - 1);
    let dbuf_line: Vec<i64> = v.dbuf[line * FB_W..line * FB_W + FB_W]
        .iter()
        .map(|&b| b as i64)
        .collect();

    let sprite = v
        .sprite
        .iter()
        .map(|s| VicSpriteSnapshot {
            data: s.data as i64,
            mc: s.mc as i64,
            mcbase: s.mcbase as i64,
            pointer: s.pointer as i64,
            exp_flop: s.exp_flop as i64,
            x: s.x as i64,
        })
        .collect();

    VicSnapshot {
        model: 0, // VICII_MODEL_MARKER (vicii-snapshot.ts:88)
        regs: v.regs.iter().map(|&b| b as i64).collect(),
        raster_cycle: v.raster_cycle as i64,
        cycle_flags: v.cycle_flags as i64,
        raster_line: v.raster_line as i64,
        start_of_frame: v.start_of_frame as i64,
        irq_status: v.irq_status as i64,
        raster_irq_line: v.raster_irq_line as i64,
        raster_irq_triggered: v.raster_irq_triggered as i64,
        vbuf: v.vbuf.iter().map(|&b| b as i64).collect(),
        cbuf: v.cbuf.iter().map(|&b| b as i64).collect(),
        gbuf: v.gbuf as i64,
        dbuf_offset: v.dbuf_offset as i64,
        dbuf: dbuf_line,
        ysmooth: v.ysmooth as i64,
        allow_bad_lines: v.allow_bad_lines as i64,
        sprite_sprite_collisions: v.sprite_sprite_collisions as i64,
        sprite_background_collisions: v.sprite_background_collisions as i64,
        clear_collisions: v.clear_collisions as i64,
        idle_state: v.idle_state as i64,
        vcbase: v.vcbase as i64,
        vc: v.vc as i64,
        rc: v.rc as i64,
        vmli: v.vmli as i64,
        bad_line: v.bad_line as i64,
        light_pen: VicLightPenSnapshot {
            // TRX64 has no light-pen state (inert in headless) — emit zeros.
            state: 0, triggered: 0, x: 0, y: 0, x_extra_bits: 0, trigger_cycle: 0,
        },
        reg11_delay: v.reg11_delay as i64,
        prefetch_cycles: v.prefetch_cycles as i64,
        sprite_display_bits: v.sprite_display_bits as i64,
        sprite_dma: v.sprite_dma as i64,
        last_color_reg: v.last_color_reg as i64,
        last_color_value: v.last_color_value as i64,
        last_read_phi1: v.last_read_phi1 as i64,
        last_bus_phi2: v.last_bus_phi2 as i64,
        vborder: v.vborder as i64,
        set_vborder: v.set_vborder as i64,
        main_border: v.main_border as i64,
        refresh_counter: v.refresh_counter as i64,
        color_ram: color_ram.iter().map(|&b| b as i64).collect(),
        sprite,
        draw_cycle: m.vic.c64re_draw_cycle_capture(),
    }
}

/// Restore TRX64's `m.vic` + color RAM from a c64re `LiteralVicSnapshot`.
/// The full-frame `dbuf` accumulator + `displayed` come from `vicPresentation`;
/// here the 520-byte draw line writes into the current `dbuf_line` row.
pub fn restore_vic(m: &mut Machine, s: &VicSnapshot) {
    {
        let v = &mut m.vic;
        for (i, &b) in s.regs.iter().enumerate().take(0x40) {
            v.regs[i] = b as u8;
        }
        v.raster_cycle = s.raster_cycle as u16;
        v.cycle_flags = s.cycle_flags as u32;
        v.raster_line = s.raster_line as u16;
        v.start_of_frame = s.start_of_frame != 0;
        v.irq_status = s.irq_status as u8;
        v.raster_irq_line = s.raster_irq_line as u16;
        v.raster_irq_triggered = s.raster_irq_triggered != 0;
        for (i, &b) in s.vbuf.iter().enumerate().take(40) {
            v.vbuf[i] = b as u8;
        }
        for (i, &b) in s.cbuf.iter().enumerate().take(40) {
            v.cbuf[i] = b as u8;
        }
        v.gbuf = s.gbuf as u8;
        v.dbuf_offset = s.dbuf_offset as usize;
        v.ysmooth = s.ysmooth as u8;
        v.allow_bad_lines = s.allow_bad_lines != 0;
        v.sprite_sprite_collisions = s.sprite_sprite_collisions as u8;
        v.sprite_background_collisions = s.sprite_background_collisions as u8;
        v.clear_collisions = s.clear_collisions as u8;
        v.idle_state = s.idle_state != 0;
        v.vcbase = s.vcbase as u16;
        v.vc = s.vc as u16;
        v.rc = s.rc as u8;
        v.vmli = s.vmli as u16;
        v.bad_line = s.bad_line != 0;
        v.reg11_delay = s.reg11_delay as u8;
        v.prefetch_cycles = s.prefetch_cycles as u8;
        v.sprite_display_bits = s.sprite_display_bits as u8;
        v.sprite_dma = s.sprite_dma as u8;
        v.last_color_reg = s.last_color_reg as u8;
        v.last_color_value = s.last_color_value as u8;
        v.last_read_phi1 = s.last_read_phi1 as u8;
        v.last_bus_phi2 = s.last_bus_phi2 as u8;
        v.vborder = s.vborder != 0;
        v.set_vborder = s.set_vborder != 0;
        v.main_border = s.main_border != 0;
        v.refresh_counter = s.refresh_counter as u8;
        for (i, sp) in s.sprite.iter().enumerate().take(crate::vic::NUM_SPRITES) {
            v.sprite[i].data = sp.data as u32;
            v.sprite[i].mc = sp.mc as u8;
            v.sprite[i].mcbase = sp.mcbase as u8;
            v.sprite[i].pointer = sp.pointer as u8;
            v.sprite[i].exp_flop = sp.exp_flop as u8;
            v.sprite[i].x = sp.x as u16;
        }
        // 520-byte current draw line into the full-frame accumulator.
        let line = v.dbuf_line.min(crate::render::FB_H - 1);
        for (i, &b) in s.dbuf.iter().enumerate().take(FB_W) {
            v.dbuf[line * FB_W + i] = b as u8;
        }
        // Re-assert the VIC IRQ line from the restored irq_status ∧ enable mask
        // (vicii-snapshot.ts:215 vicii_irq_set_line()).
        v.irq_line = (v.irq_status & v.regs[0x1a] & 0x0f) != 0;
    }
    m.vic.c64re_draw_cycle_restore(&s.draw_cycle);
    write_color_ram(m, &s.color_ram.iter().map(|&b| b as u8).collect::<Vec<u8>>());
}

/// Read the `vicPresentation` seam: the two 520×312 color-index framebuffers
/// (literalPortFb = the live `dbuf` accumulator, literalPortFbStable = the last
/// complete `displayed` frame) + the continuation scalars.
pub fn capture_vic_presentation(m: &Machine) -> VicPresentationSnapshot {
    VicPresentationSnapshot {
        literal_port_fb: ta_u8(m.vic.dbuf.as_ref()),
        literal_port_fb_stable: ta_u8(m.vic.displayed.as_ref()),
        lit_last_raster_line: m.vic.dbuf_line as i64,
        last_lit_ba_low: m.vic.ba_low_flag as i64,
        lit_stable_frame_count: m.vic.frame as i64,
    }
}

/// Restore the `vicPresentation` framebuffers + scalars.
pub fn restore_vic_presentation(m: &mut Machine, p: &VicPresentationSnapshot) {
    if let Some(fb) = ta_u8_decode(&p.literal_port_fb) {
        let n = fb.len().min(m.vic.dbuf.len());
        m.vic.dbuf[0..n].copy_from_slice(&fb[0..n]);
    }
    if let Some(fb) = ta_u8_decode(&p.literal_port_fb_stable) {
        let n = fb.len().min(m.vic.displayed.len());
        m.vic.displayed[0..n].copy_from_slice(&fb[0..n]);
    }
    m.vic.dbuf_line = p.lit_last_raster_line.max(0) as usize;
    m.vic.ba_low_flag = p.last_lit_ba_low != 0;
    m.vic.frame = p.lit_stable_frame_count.max(0) as u64;
    let _ = (ta_u32, ta_u32_decode); // used by the draw-cycle codec in vic.rs
}

// ── CIA register helper (re-export for the daemon load path) ────────────────────

/// Recompute TRX64 timer latches from the restored register file. Used by the
/// daemon restore so the CIA TAL/TAH/TBL/TBH bytes feed the live counters even
/// when a cross-runtime dump only carried the register file.
pub fn reseed_cia_timer_latches(cia: &mut Cia) {
    let tal = cia.regs[CIA_TAL] as u16;
    let tah = cia.regs[CIA_TAH] as u16;
    let latch_a = tal | (tah << 8);
    if latch_a != 0 {
        cia.ta.latch = latch_a;
    }
    let tbl = cia.regs[CIA_TBL] as u16;
    let tbh = cia.regs[CIA_TBH] as u16;
    let latch_b = tbl | (tbh << 8);
    if latch_b != 0 {
        cia.tb.latch = latch_b;
    }
    // touch the TOD/SDR/CR consts so the import set stays meaningful + greppable
    let _ = (CIA_TOD_TEN, CIA_TOD_SEC, CIA_TOD_MIN, CIA_TOD_HR, CIA_SDR, CIA_CRA, CIA_CRB);
}

// ── color-RAM helper ────────────────────────────────────────────────────────────

/// TRX64 keeps color RAM in the 64K image at $D800-$DBFF (low nibble per cell).
/// The c64re VIC snapshot carries the 0x400-byte color RAM as a SEPARATE field
/// (it is not in the c64re 64K RAM image). Read it for the VIC capture (part 3).
pub fn read_color_ram(m: &Machine) -> [u8; 0x400] {
    let mut out = [0u8; 0x400];
    for (i, c) in out.iter_mut().enumerate() {
        *c = m.ram[0xd800 + i] & 0x0f;
    }
    out
}

/// Write color RAM back (low nibble) into BOTH stores that must agree:
/// - `ram[$D800..]` — the ISO-bus VIC + what `read_color_ram` captures;
/// - `io_shadow[$0800..]` — the FULL-machine bus VIC's colour-RAM source
///   (`full.rs` reads colour RAM from `io_shadow`, NOT `ram`).
/// Writing only `ram` left every product (full-machine) restore blind to colour
/// RAM: each "11" multicolor-bitmap pixel resolved to colour 0 (black), so a
/// resumed screen rendered its white text/HUD black. This is the single colour-RAM
/// restore primitive for `.c64re`, the ring, and the VSF import.
pub fn write_color_ram(m: &mut Machine, color_ram: &[u8]) {
    for (i, &c) in color_ram.iter().enumerate().take(0x400) {
        m.ram[0xd800 + i] = (m.ram[0xd800 + i] & 0xf0) | (c & 0x0f);
        m.io_shadow[0x0800 + i] = (m.io_shadow[0x0800 + i] & 0xf0) | (c & 0x0f);
    }
}

// ── cartState (Spec 792.1 — cartridge continuation) ─────────────────────────────
//
// The confirmed gap (Spec 792 §Problem): the checkpoint re-created the cart mapper
// from `cartBytes` + overlaid `cartFlash` (the flash DATA array) but NEVER captured
// the mapper's live continuation — `current_bank`, `control_register`
// (EF register_02 / MegaCart mode), the EasyFlash jumper, the IO2 RAM, and the
// flash command-FSM. A banked cart therefore resumed at bank 0 / register 0.
//
// The mapper already exposes `get_state()`/`set_state()` (`CartState` in cart.rs).
// This node serializes that state (the SAME shape c64re's `HeadlessCartridgeState`
// carries — types.ts:54-94, camelCase field names) so both `.c64re` and the ring
// round-trip the full cart continuation. Additive + `Option`-gated: a `.c64re`
// without `cartState` (707 back-compat) still loads (restore skips a null/absent
// node), and a read-only cart emits only `currentBank`/`controlRegister`.

use crate::cart::{CartMapper, CartState, FlashCartState};
use crate::flash040::Flash040SnapState;
use crate::m93c86::M93c86SnapState;

/// Flash040 command-FSM continuation (= c64re `Flash040SnapState`, types.ts:86-94).
/// The flash DATA array is NOT here — it rides in the separate `cartFlash` writable
/// image; only the command state + pending erase-alarm clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flash040StateSnapshot {
    pub state: i64,
    #[serde(rename = "baseState")]
    pub base_state: i64,
    #[serde(rename = "programByte")]
    pub program_byte: i64,
    #[serde(rename = "lastRead")]
    pub last_read: i64,
    pub dirty: bool,
    #[serde(rename = "eraseMask")]
    pub erase_mask: Vec<i64>, // 8 bytes
    #[serde(rename = "eraseAlarmClk")]
    pub erase_alarm_clk: i64,
}

/// M93C86 serial-EEPROM continuation (= c64re `M93c86SnapState`, m93c86.ts:19-26) —
/// the full 2KB array + the serial command shift state. GMOD2 / C64MegaCart carry
/// this alongside `flashLoState`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M93c86StateSnapshot {
    pub data: Vec<i64>,
    pub cs: i64,
    pub clock: i64,
    #[serde(rename = "dataIn")]
    pub data_in: i64,
    #[serde(rename = "dataOut")]
    pub data_out: i64,
    #[serde(rename = "inputShiftreg")]
    pub input_shiftreg: i64,
    #[serde(rename = "inputCount")]
    pub input_count: i64,
    #[serde(rename = "outputShiftreg")]
    pub output_shiftreg: i64,
    #[serde(rename = "outputCount")]
    pub output_count: i64,
    pub command: i64,
    pub addr: i64,
    #[serde(rename = "writeEnable")]
    pub write_enable: i64,
    #[serde(rename = "readyBusy")]
    pub ready_busy: i64,
}

/// The mapper continuation (= c64re `HeadlessCartridgeState`, types.ts:54-94, the
/// checkpoint-relevant subset). `currentBank`/`controlRegister` for every family;
/// the EasyFlash jumper + IO2 RAM + per-flash command state only for the writable
/// tier (`skip_serializing_if` keeps the read-only node minimal).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CartStateSnapshot {
    #[serde(rename = "currentBank")]
    pub current_bank: i64,
    #[serde(rename = "controlRegister", skip_serializing_if = "Option::is_none")]
    pub control_register: Option<i64>,
    #[serde(rename = "easyflashJumper", skip_serializing_if = "Option::is_none")]
    pub easyflash_jumper: Option<i64>,
    #[serde(rename = "easyflashRam", skip_serializing_if = "Option::is_none")]
    pub easyflash_ram: Option<Vec<i64>>, // 256 bytes IO2 RAM
    #[serde(rename = "flashLoState", skip_serializing_if = "Option::is_none")]
    pub flash_lo_state: Option<Flash040StateSnapshot>,
    #[serde(rename = "flashHiState", skip_serializing_if = "Option::is_none")]
    pub flash_hi_state: Option<Flash040StateSnapshot>,
    #[serde(rename = "eepromState", skip_serializing_if = "Option::is_none")]
    pub eeprom_state: Option<M93c86StateSnapshot>,
}

fn flash_state_to_json(s: &Flash040SnapState) -> Flash040StateSnapshot {
    Flash040StateSnapshot {
        state: s.state as i64,
        base_state: s.base_state as i64,
        program_byte: s.program_byte as i64,
        last_read: s.last_read as i64,
        dirty: s.dirty,
        erase_mask: s.erase_mask.iter().map(|&b| b as i64).collect(),
        erase_alarm_clk: s.erase_alarm_clk,
    }
}

fn flash_state_from_json(s: &Flash040StateSnapshot) -> Flash040SnapState {
    // erase_mask is a fixed 8-byte array (flash040 FLASH040_ERASE_MASK_SIZE).
    let mut mask = [0u8; 8];
    for (i, &b) in s.erase_mask.iter().enumerate().take(8) {
        mask[i] = b as u8;
    }
    Flash040SnapState {
        state: s.state as u8,
        base_state: s.base_state as u8,
        program_byte: s.program_byte as u8,
        last_read: s.last_read as u8,
        dirty: s.dirty,
        erase_mask: mask,
        erase_alarm_clk: s.erase_alarm_clk,
    }
}

fn eeprom_state_to_json(s: &M93c86SnapState) -> M93c86StateSnapshot {
    M93c86StateSnapshot {
        data: s.data.iter().map(|&b| b as i64).collect(),
        cs: s.cs as i64,
        clock: s.clock as i64,
        data_in: s.data_in as i64,
        data_out: s.data_out as i64,
        input_shiftreg: s.input_shiftreg as i64,
        input_count: s.input_count as i64,
        output_shiftreg: s.output_shiftreg as i64,
        output_count: s.output_count as i64,
        command: s.command as i64,
        addr: s.addr as i64,
        write_enable: s.write_enable as i64,
        ready_busy: s.ready_busy as i64,
    }
}

fn eeprom_state_from_json(s: &M93c86StateSnapshot) -> M93c86SnapState {
    M93c86SnapState {
        data: s.data.iter().map(|&b| b as u8).collect(),
        cs: s.cs as u8,
        clock: s.clock as u8,
        data_in: s.data_in as u8,
        data_out: s.data_out as u8,
        input_shiftreg: s.input_shiftreg as u32,
        input_count: s.input_count as u32,
        output_shiftreg: s.output_shiftreg as u32,
        output_count: s.output_count as u32,
        command: s.command as u8,
        addr: s.addr as u32,
        write_enable: s.write_enable as u8,
        ready_busy: s.ready_busy as u8,
    }
}

/// Read a live cartridge mapper's continuation (`get_state()`) into the `cartState`
/// snapshot shape. `get_state()` is `&self` (it clones the flash + snapshots the FSM
/// at clk 0, exactly like the VSF cart-state capture), so this is a pure read.
pub fn capture_cart_state(cart: &dyn CartMapper) -> CartStateSnapshot {
    let st = cart.get_state();
    let (jumper, ram, lo, hi, eeprom) = match &st.flash {
        Some(f) => (
            Some(f.easyflash_jumper as i64),
            if f.easyflash_ram.is_empty() {
                None
            } else {
                Some(f.easyflash_ram.iter().map(|&b| b as i64).collect())
            },
            f.flash_lo.as_ref().map(flash_state_to_json),
            f.flash_hi.as_ref().map(flash_state_to_json),
            f.eeprom.as_ref().map(eeprom_state_to_json),
        ),
        None => (None, None, None, None, None),
    };
    CartStateSnapshot {
        current_bank: st.current_bank as i64,
        control_register: st.control_register.map(|v| v as i64),
        easyflash_jumper: jumper,
        easyflash_ram: ram,
        flash_lo_state: lo,
        flash_hi_state: hi,
        eeprom_state: eeprom,
    }
}

/// Restore a live cartridge mapper's continuation from the `cartState` snapshot
/// (`set_state()`), AFTER the mapper has been re-created from `cartBytes` + the flash
/// DATA overlaid from `cartFlash`. The read-only mappers consume only
/// `currentBank`/`controlRegister`; the writable tier also re-arms the flash FSM +
/// jumper + IO2 RAM + EEPROM.
pub fn restore_cart_state(cart: &mut Box<dyn CartMapper>, s: &CartStateSnapshot) {
    let has_flash = s.flash_lo_state.is_some()
        || s.flash_hi_state.is_some()
        || s.eeprom_state.is_some()
        || s.easyflash_ram.is_some()
        || s.easyflash_jumper.is_some();
    let flash = if has_flash {
        Some(FlashCartState {
            flash_lo: s.flash_lo_state.as_ref().map(flash_state_from_json),
            flash_hi: s.flash_hi_state.as_ref().map(flash_state_from_json),
            eeprom: s.eeprom_state.as_ref().map(eeprom_state_from_json),
            easyflash_jumper: s.easyflash_jumper.unwrap_or(0) as u8,
            easyflash_ram: s
                .easyflash_ram
                .as_ref()
                .map(|v| v.iter().map(|&b| b as u8).collect())
                .unwrap_or_default(),
        })
    } else {
        None
    };
    cart.set_state(CartState {
        current_bank: s.current_bank as u16,
        control_register: s.control_register.map(|v| v as u8),
        flash,
    });
}

// ── RAM as a `$ta` node ─────────────────────────────────────────────────────────

/// Encode the 64K RAM as the c64re `{ $ta: "Uint8Array", b64 }` node.
pub fn ram_ta(m: &Machine) -> serde_json::Value {
    ta_u8(m.ram.as_ref())
}

/// Decode a `{ $ta }` RAM node into the 64K image. Returns false on a bad node.
pub fn restore_ram_ta(m: &mut Machine, node: &serde_json::Value) -> bool {
    if let Some(bytes) = ta_u8_decode(node) {
        let n = bytes.len().min(0x10000);
        m.ram[0..n].copy_from_slice(&bytes[0..n]);
        // Recompute memconfig from the restored port latches (set separately).
        true
    } else {
        false
    }
}

// ── Top-level RuntimeCheckpoint orchestrator ───────────────────────────────────
//
// Assembles the COMPLETE c64re RuntimeCheckpoint JSON tree (headless-machine-
// kernel.ts:946-997 capture()), in the exact field order/names, from the live
// TRX64 `Machine`. The result is the `checkpoint` payload handed to
// `native_snapshot::write_native_snapshot`. Typed-array fields (ram, framebuffers,
// drive blob, draw-cycle Uint8/32Arrays) are already `{ $ta }`-tagged so the
// container round-trips them byte-for-byte.
//
// Field coverage vs runtime-checkpoint.ts:
//   DONE (this batch): schemaVersion, atInstructionBoundary, cpu, ram,
//     cpuPortDirection, cpuPortValue, cia1, cia2, sid, iec, cpuIntStatus,
//     keyboard, joystick1/2, paddles, vic, vicPresentation, vicProvenance(null),
//     media, alarmsMaincpu([]), audio(null).
//   PENDING (part 4): drive1541 (+ driveDiskImage) — emitted null until the VICE
//     drive snapshot-module blob lands; c64re restore tolerates null (drive kept).

/// Build the full RuntimeCheckpoint payload tree from the live machine.
/// `disk_path`/`image_format` feed the `media` metadata (the daemon supplies them
/// from the attached disk). `drive1541` is the optional VICE drive blob.
///
/// `cart_bytes`/`cart_flash` (Spec 714.5, formats-state-2) are the attached
/// cartridge's original `.crt` bytes + mutable writable image (flash low+high), which
/// the daemon captures from the live mapper BEFORE this immutable-borrow call (the
/// writable image read needs `&mut` to catch the flash erase alarm up). They mirror
/// c64re's headless-machine-kernel.ts:988-989 `captureCartBytes()`/`captureCartFlash()`
/// — non-null whenever a cartridge is attached (cart_bytes) / has a writable port
/// (cart_flash); None ⇒ no cartridge / read-only mapper. Lost-on-dump before this fix
/// (both were hardcoded null).
pub fn capture_runtime_checkpoint(
    m: &Machine,
    disk_path: &str,
    image_format: &str,
    drive1541: Option<&[u8]>,
    drive_disk_image: Option<&[u8]>,
    cart_bytes: Option<&[u8]>,
    cart_flash: Option<&[u8]>,
) -> serde_json::Value {
    use serde_json::json;
    let keys = m.keyboard.pressed_keys();
    json!({
        "schemaVersion": RUNTIME_CHECKPOINT_SCHEMA_VERSION,
        "atInstructionBoundary": true,
        "cpu": serde_json::to_value(capture_cpu(m)).unwrap(),
        "ram": ram_ta(m),
        "cpuPortDirection": m.port_dir as i64,
        "cpuPortValue": m.port_data as i64,
        "cia1": serde_json::to_value(capture_cia(&m.cia1)).unwrap(),
        "cia2": serde_json::to_value(capture_cia(&m.cia2)).unwrap(),
        "sid": serde_json::to_value(capture_sid(m)).unwrap(),
        "iec": serde_json::to_value(capture_iec(m)).unwrap(),
        "cpuIntStatus": serde_json::to_value(capture_int_status(m)).unwrap(),
        // maincpu alarm schedule: TRX64's maincpu is NOT alarm-driven (distilled
        // IntStatus) — the drive's VIA alarms ride the drive blob (part 4), exactly
        // as runtime-checkpoint.ts:137-144 documents. Empty schedule.
        "alarmsMaincpu": [],
        "keyboard": { "livePressed": keys },
        "joystick1": { "up": false, "down": false, "left": false, "right": false, "fire": false },
        "joystick2": { "up": false, "down": false, "left": false, "right": false, "fire": false },
        "paddles": [0, 0, 0, 0],
        "vic": serde_json::to_value(capture_vic(m)).unwrap(),
        "vicPresentation": serde_json::to_value(capture_vic_presentation(m)).unwrap(),
        "vicProvenance": serde_json::Value::Null,
        "drive1541": drive1541.map(ta_u8).unwrap_or(serde_json::Value::Null),
        "driveDiskImage": drive_disk_image.map(ta_u8).unwrap_or(serde_json::Value::Null),
        // Spec 714.5 (formats-state-2): the attached cartridge's original .crt bytes +
        // mutable flash image, as `{ $ta }` typed-array nodes (null = no cart / no
        // writable port), so dump/undump round-trips a written EasyFlash's flash.
        "cartBytes": cart_bytes.map(ta_u8).unwrap_or(serde_json::Value::Null),
        "cartFlash": cart_flash.map(ta_u8).unwrap_or(serde_json::Value::Null),
        // Spec 792.1 — the cart mapper's live continuation (bank/register/jumper/IO2
        // RAM/flash-FSM), read from the live mapper via `get_state()` (`&self`). Null
        // when no cartridge is attached. Additive: a `.c64re` without this node still
        // loads (707 back-compat). Rides the ring too (checkpoint_ring stores the same
        // tree + restores via `restore_runtime_checkpoint`).
        "cartState": m
            .cartridge
            .as_ref()
            .map(|c| serde_json::to_value(capture_cart_state(c.as_ref())).unwrap())
            .unwrap_or(serde_json::Value::Null),
        "media": { "diskPath": disk_path, "imageFormat": image_format },
        "audio": serde_json::Value::Null,
    })
}

/// Restore the full machine from a RuntimeCheckpoint payload tree. Order mirrors
/// the kernel restore (headless-machine-kernel.ts:1005-1084): RAM → CPU-port
/// (re-runs PLA banking) → CPU regs → CIA → SID → IEC → IRQ status → input →
/// literal VIC → VIC presentation. Returns the decoded drive1541 blob (if any) so
/// the caller (daemon, part 5) can hand it to the drive restore.
///
/// Returns Err on a malformed/incompatible payload (no partial restore).
pub fn restore_runtime_checkpoint(
    m: &mut Machine,
    cp: &serde_json::Value,
) -> Result<Option<Vec<u8>>, String> {
    let schema = cp.get("schemaVersion").and_then(|v| v.as_i64()).unwrap_or(-1);
    if schema != RUNTIME_CHECKPOINT_SCHEMA_VERSION {
        return Err(format!(
            "restore: unexpected checkpoint schemaVersion {schema} (want {RUNTIME_CHECKPOINT_SCHEMA_VERSION})"
        ));
    }

    // RAM + CPU port (recompute PLA memconfig).
    if let Some(node) = cp.get("ram") {
        restore_ram_ta(m, node);
    }
    if let Some(d) = cp.get("cpuPortDirection").and_then(|v| v.as_i64()) {
        m.port_dir = d as u8;
    }
    if let Some(d) = cp.get("cpuPortValue").and_then(|v| v.as_i64()) {
        m.port_data = d as u8;
    }
    let port = ((!m.port_dir | m.port_data) & 0x07) as usize;
    m.memconfig = m.memconfig_table[(port | 0x18) & 0x1f];

    // CPU.
    if let Some(c) = cp.get("cpu") {
        let cpu: CpuSnapshot =
            serde_json::from_value(c.clone()).map_err(|e| format!("restore cpu: {e}"))?;
        restore_cpu(m, &cpu);
    }

    // CIA1 / CIA2.
    if let Some(c) = cp.get("cia1") {
        let s: CiaSnapshot =
            serde_json::from_value(c.clone()).map_err(|e| format!("restore cia1: {e}"))?;
        restore_cia(&mut m.cia1, &s);
        reseed_cia_timer_latches(&mut m.cia1);
    }
    if let Some(c) = cp.get("cia2") {
        let s: CiaSnapshot =
            serde_json::from_value(c.clone()).map_err(|e| format!("restore cia2: {e}"))?;
        restore_cia(&mut m.cia2, &s);
        reseed_cia_timer_latches(&mut m.cia2);
    }

    // SID.
    if let Some(c) = cp.get("sid") {
        let s: SidSnapshot =
            serde_json::from_value(c.clone()).map_err(|e| format!("restore sid: {e}"))?;
        restore_sid(m, &s);
    }

    // IEC + IRQ status.
    if let Some(c) = cp.get("iec") {
        let s: IecSnapshot =
            serde_json::from_value(c.clone()).map_err(|e| format!("restore iec: {e}"))?;
        restore_iec(m, &s);
    }
    if let Some(c) = cp.get("cpuIntStatus") {
        let s: IntStatusSnapshot =
            serde_json::from_value(c.clone()).map_err(|e| format!("restore cpuIntStatus: {e}"))?;
        restore_int_status(m, &s);
    }

    // Keyboard (input).
    m.keyboard.release_keys();
    if let Some(keys) = cp
        .get("keyboard")
        .and_then(|k| k.get("livePressed"))
        .and_then(|v| v.as_array())
    {
        for k in keys.iter().filter_map(|v| v.as_str()) {
            m.keyboard.key_down(k);
        }
    }

    // Literal VIC + presentation.
    if let Some(c) = cp.get("vic") {
        let s: VicSnapshot =
            serde_json::from_value(c.clone()).map_err(|e| format!("restore vic: {e}"))?;
        restore_vic(m, &s);
    }
    if let Some(c) = cp.get("vicPresentation") {
        let s: VicPresentationSnapshot =
            serde_json::from_value(c.clone()).map_err(|e| format!("restore vicPresentation: {e}"))?;
        restore_vic_presentation(m, &s);
    }

    // Sync the legacy shadow + machine clk (matches vsf load tail).
    m.sync_after_monitor();
    m.clk = m.c64_core.clk;

    // Drive restore (part 4): the `drive1541` core blob (DRIVE8/DRIVECPU0/VIA1/VIA2)
    // then the `driveDiskImage` GCRIMAGE0 overlay. The caller (daemon) has already
    // re-attached the embedded disk before this point, so the drive's GCR baseline
    // is present; `restore_drive_disk_image` overlays the mutable content (§6.1
    // mutable-wins). A null/absent drive blob leaves the drive at its baseline.
    let drive_blob = cp.get("drive1541").and_then(ta_u8_decode);
    if let Some(ref blob) = drive_blob {
        crate::drive_snapshot::restore_drive1541(&mut m.drive8, blob)?;
    }
    if let Some(disk_blob) = cp.get("driveDiskImage").and_then(ta_u8_decode) {
        crate::drive_snapshot::restore_drive_disk_image(&mut m.drive8, &disk_blob)?;
    }

    // Re-anchor the drive's C64-clock catch-up reference to the restored anchor
    // instant (= the restored C64 clk). `drive_c64_ref` is the monotonic C64 clock
    // the drive was last advanced up to; the next push-flush catch-up advances the
    // drive by `clk - drive_c64_ref`. It is a Machine-level field (NOT part of the
    // drive blob — the blob carries the drive CPU's own `stop_clk`/`cycle_accum`),
    // so a restore that left it at its STALE pre-restore value made the first
    // post-restore catch-up feed the drive the wrong number of cycles — a
    // non-deterministic drive replay (restore A + run N twice landed the drive 6502
    // at a different PC each time, ±a few cycles from the fixed-point sync_accum
    // phase). VICE/TS keep this reference inside the drive CPU (`cpu->last_clk` in
    // the DRIVECPU CLOCK chunk) so it re-anchors implicitly; TRX64's split needs
    // this explicit re-anchor. Set it to the restored C64 clk so the drive resumes
    // exactly where it was captured. (Spec 761 §5.3 deterministic-replay.)
    m.drive_c64_ref = m.c64_core.clk;

    // Cartridge restore (Spec 714.5 / formats-state-2): recreate the mapper from the
    // captured original `.crt` bytes, then overlay the mutable flash image — mirroring
    // c64re's restoreMediaCheckpoint(cp.media, cp.cartBytes, cp.cartFlash)
    // (headless-machine-kernel.ts:1071/1126-1134). `cartBytes` null ⇒ no cartridge was
    // attached → detach (matches the TS `attachCartridge(undefined)` branch). The
    // parsed cart name comes from the `.crt` header; the placeholder path is only the
    // empty-name fallback (parse_crt name handling).
    match cp.get("cartBytes").and_then(ta_u8_decode) {
        Some(cart_bytes) if !cart_bytes.is_empty() => {
            m.attach_cart_from_bytes(&cart_bytes, "snapshot.crt")
                .map_err(|e| format!("restore cart: {e:?}"))?;
            // Overlay the mutable writable image (flash low+high) when captured. This
            // restores the flash DATA array (Spec 714.5); the flash COMMAND-FSM +
            // bank/register/jumper/IO2-RAM come from `cartState` below.
            if let Some(flash) = cp.get("cartFlash").and_then(ta_u8_decode) {
                if !flash.is_empty() {
                    if let Some(cart) = m.cartridge.as_mut() {
                        cart.set_writable_image(&flash);
                    }
                }
            }
            // Spec 792.1 — re-arm the mapper continuation (bank/control-register/
            // jumper/IO2-RAM/flash-FSM) so a banked cart resumes at its captured bank,
            // not bank 0. AFTER attach + the flash-DATA overlay (mirrors the VSF EF
            // restore, vsf.rs:944-955). Null/absent ⇒ a pre-792 `.c64re`: leave the
            // freshly-attached mapper at its boot state (707 back-compat).
            match cp.get("cartState") {
                Some(cs) if !cs.is_null() => {
                    let snap: CartStateSnapshot = serde_json::from_value(cs.clone())
                        .map_err(|e| format!("restore cartState: {e}"))?;
                    if let Some(cart) = m.cartridge.as_mut() {
                        restore_cart_state(cart, &snap);
                    }
                    // The restored register/jumper change EXROM/GAME (the attach ran
                    // with register_02 = 0) → recompute the live memconfig from the
                    // restored cart lines (mirrors vsf.rs:959).
                    m.memconfig = m.memconfig_table[m.pla_index()];
                }
                _ => {}
            }
        }
        _ => {
            // No cartridge in the checkpoint → ensure none is attached.
            m.detach_cart();
        }
    }

    // Hand back the decoded `drive1541` blob for the caller's logging/diagnostics.
    Ok(drive_blob)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_checkpoint_roundtrip() {
        let mut m = Machine::new();
        // Seed a recognizable state across every chip.
        m.c64_core.reg_pc = 0xc000;
        m.c64_core.reg_a = 0x42;
        m.c64_core.clk = 1_234_567;
        m.ram[0x0400] = 0x08;
        m.ram[0xd800] = 0x0e; // color RAM
        m.port_dir = 0x2f;
        m.port_data = 0x17;
        m.cia1.regs[CIA_TAL] = 0x11;
        m.cia1.irqflags = 0x81;
        m.cia2.regs[0] = 0x3f;
        m.sid_regs[0x18] = 0x0f;
        m.iec.iecbus.cpu_bus = 0x55;
        m.vic.regs[0x11] = 0x1b;
        m.vic.raster_line = 100;
        m.c64_int.nirq = 1;
        m.c64_int.pending_int = [0, 0x02, 0, 0];
        m.keyboard.key_down("A");

        let cp = capture_runtime_checkpoint(&m, "/tmp/x.g64", "g64", None, None, None, None);
        // Spot-check the tree shape (field names match c64re).
        assert_eq!(cp["schemaVersion"], 1);
        assert_eq!(cp["atInstructionBoundary"], true);
        assert_eq!(cp["cpu"]["pc"], 0xc000);
        assert_eq!(cp["cpuPortValue"], 0x17);
        assert_eq!(cp["cia1"]["v"], 2);
        assert_eq!(cp["sid"]["v"], 2);
        assert_eq!(cp["iec"]["cpu_bus"], 0x55);
        assert_eq!(cp["vic"]["raster_line"], 100);
        assert_eq!(cp["media"]["diskPath"], "/tmp/x.g64");
        assert_eq!(cp["drive1541"], serde_json::Value::Null);
        assert!(cp["alarmsMaincpu"].as_array().unwrap().is_empty());
        assert_eq!(cp["keyboard"]["livePressed"][0], "A");

        // Round-trip through a fresh machine.
        let mut m2 = Machine::new();
        let blob = restore_runtime_checkpoint(&mut m2, &cp).expect("restore");
        assert!(blob.is_none());
        assert_eq!(m2.c64_core.reg_pc, 0xc000);
        assert_eq!(m2.c64_core.reg_a, 0x42);
        assert_eq!(m2.c64_core.clk, 1_234_567);
        assert_eq!(m2.ram[0x0400], 0x08);
        assert_eq!(m2.ram[0xd800] & 0x0f, 0x0e);
        assert_eq!(m2.port_dir, 0x2f);
        assert_eq!(m2.port_data, 0x17);
        assert_eq!(m2.cia1.regs[CIA_TAL], 0x11);
        assert_eq!(m2.cia1.irqflags, 0x81);
        assert_eq!(m2.cia2.regs[0], 0x3f);
        assert_eq!(m2.sid_regs[0x18], 0x0f);
        assert_eq!(m2.iec.iecbus.cpu_bus, 0x55);
        assert_eq!(m2.vic.regs[0x11], 0x1b);
        assert_eq!(m2.vic.raster_line, 100);
        assert_eq!(m2.c64_int.pending_int[1], 0x02);
        assert_eq!(m2.keyboard.pressed_keys(), vec!["A".to_string()]);
    }

    #[test]
    fn full_checkpoint_through_container() {
        // The full path: capture → write_native_snapshot → read_native_snapshot →
        // restore. This proves the structured checkpoint survives the .c64re
        // binary container (gzip + sha + `$ta` codec) intact.
        use crate::native_snapshot::{
            read_native_snapshot, write_native_snapshot, WriteNativeSnapshotArgs,
        };
        let mut m = Machine::new();
        m.c64_core.reg_pc = 0xabcd;
        m.ram[0x2000] = 0x99;
        m.vic.raster_line = 55;

        let cp = capture_runtime_checkpoint(&m, "", "", None, None, None, None);
        let bytes = write_native_snapshot(WriteNativeSnapshotArgs {
            checkpoint: cp,
            schema_version: 1,
            media: vec![],
            runtime_version: "trx64/1".into(),
            machine_model: "c64-pal".into(),
            provenance: None,
            pc: 0xabcd,
            cycle: m.c64_core.clk as i64,
        });
        let r = read_native_snapshot(&bytes).expect("read");
        let mut m2 = Machine::new();
        restore_runtime_checkpoint(&mut m2, &r.checkpoint).expect("restore");
        assert_eq!(m2.c64_core.reg_pc, 0xabcd);
        assert_eq!(m2.ram[0x2000], 0x99);
        assert_eq!(m2.vic.raster_line, 55);
    }

    #[test]
    fn drive_blob_through_container_roundtrip() {
        // Part 4 — the `drive1541` + `driveDiskImage` blobs survive the full
        // `.c64re` container. Seed recognizable drive state (CPU regs, RAM, rotation
        // head, a GCR track byte), capture both blobs, run through the container,
        // restore, and assert the drive resumed (no drive_snapshot_read corruption).
        use crate::drive::{DiskImage, DiskKind};
        use crate::gcr::{GcrImage, GcrTrack};
        use crate::native_snapshot::{
            read_native_snapshot, write_native_snapshot, WriteNativeSnapshotArgs,
        };

        let mut m = Machine::new();
        // Seed drive CPU + RAM.
        m.drive8.core.reg_pc = 0xf2b0;
        m.drive8.core.reg_a = 0x37;
        m.drive8.core.reg_sp = 0xf8;
        m.drive8.core.clk = 9_876_543;
        m.drive8.drive_ram_write(0x0050, 0x5a);
        m.drive8.drive_ram_write(0x07ff, 0xa5);

        // Attach a GCR image directly (no boot needed) + seed a head + track byte.
        let mut tracks: Vec<GcrTrack> = (0..84)
            .map(|_| GcrTrack { data: vec![0u8; 7000], size: 7000 })
            .collect();
        // Track 18 (half-track 36 → slot 34) gets a marker byte.
        tracks[34].data[100] = 0xc9;
        m.drive8.rotation.image = Some(GcrImage { tracks });
        m.drive8.rotation.gcr_image_loaded = 1;
        m.drive8.rotation.complicated_image_loaded = 1;
        m.drive8.rotation.current_half_track = 36;
        m.drive8.rotation.gcr_current_track_size = 7000;
        m.drive8.rotation.gcr_head_offset = 1234;
        // At an instruction boundary the rotation clock tracks the drive clock, so
        // the VIA undump's rotate_disk advances 0 bits (no head drift on restore).
        m.drive8.rotation.rotation_last_clk = m.drive8.core.clk;
        m.drive8.disk = Some(DiskImage {
            kind: DiskKind::D64,
            bytes: vec![0u8; 174848],
            backing_path: None,
            read_only: false,
        });

        // Capture both drive blobs + the full checkpoint.
        let drive1541 = crate::drive_snapshot::capture_drive1541(&mut m.drive8);
        let disk_blob = crate::drive_snapshot::capture_drive_disk_image(&m.drive8);
        assert!(disk_blob.is_some(), "GCRIMAGE0 blob present");
        let cp = capture_runtime_checkpoint(
            &m,
            "/tmp/d.d64",
            "d64",
            Some(&drive1541),
            disk_blob.as_deref(),
            None,
            None,
        );
        assert_ne!(cp["drive1541"], serde_json::Value::Null);
        assert_ne!(cp["driveDiskImage"], serde_json::Value::Null);

        let bytes = write_native_snapshot(WriteNativeSnapshotArgs {
            checkpoint: cp,
            schema_version: 1,
            media: vec![],
            runtime_version: "trx64/1".into(),
            machine_model: "c64-pal".into(),
            provenance: None,
            pc: 0,
            cycle: 0,
        });
        let r = read_native_snapshot(&bytes).expect("read container");

        // Restore into a fresh machine WITH a baseline disk attached (daemon order:
        // the embedded media is re-attached before restore_runtime_checkpoint).
        let mut m2 = Machine::new();
        m2.drive8.attach_disk(DiskImage {
            kind: DiskKind::D64,
            bytes: vec![0u8; 174848],
            backing_path: None,
            read_only: false,
        });
        restore_runtime_checkpoint(&mut m2, &r.checkpoint).expect("restore");

        // The drive resumed: CPU + RAM + head + the mutable GCR byte survived.
        assert_eq!(m2.drive8.core.reg_pc, 0xf2b0, "drive PC");
        assert_eq!(m2.drive8.core.reg_a, 0x37, "drive A");
        assert_eq!(m2.drive8.core.reg_sp, 0xf8, "drive SP");
        assert_eq!(m2.drive8.core.clk, 9_876_543, "drive CLK");
        assert_eq!(m2.drive8.drive_ram_read(0x0050), 0x5a, "drive RAM $50");
        assert_eq!(m2.drive8.drive_ram_read(0x07ff), 0xa5, "drive RAM $7ff");
        assert_eq!(m2.drive8.rotation.current_half_track, 36, "head half-track");
        assert_eq!(m2.drive8.rotation.gcr_head_offset, 1234, "GCR head offset");
        assert_eq!(
            m2.drive8.rotation.image.as_ref().unwrap().tracks[34].data[100],
            0xc9,
            "mutable GCR track byte survived the round-trip"
        );
    }

    #[test]
    fn cia_roundtrip_register_and_timers() {
        let mut m = Machine::new();
        m.cia1.regs[CIA_TAL] = 0x34;
        m.cia1.regs[CIA_TAH] = 0x12;
        m.cia1.ta.state = 0x55;
        m.cia1.ta.latch = 0x1234;
        m.cia1.ta.cnt = 0x0abc;
        m.cia1.ta.clk = 9999;
        m.cia1.irqflags = 0x83;
        m.cia1.clk = 4242;

        let snap = capture_cia(&m.cia1);
        assert_eq!(snap.v, 2);
        assert_eq!(snap.c_cia.len(), 16);
        assert_eq!(snap.ta_state, 0x55);
        assert_eq!(snap.ta_latch, 0x1234);
        assert_eq!(snap.read_clk, 4242);
        assert_eq!(snap.old_pa, 0xff);
        assert_eq!(snap.model, 0);

        let mut m2 = Machine::new();
        restore_cia(&mut m2.cia1, &snap);
        assert_eq!(m2.cia1.regs[CIA_TAL], 0x34);
        assert_eq!(m2.cia1.ta.state, 0x55);
        assert_eq!(m2.cia1.ta.latch, 0x1234);
        assert_eq!(m2.cia1.ta.cnt, 0x0abc);
        assert_eq!(m2.cia1.irqflags, 0x83);
        assert_eq!(m2.cia1.clk, 4242);
    }

    #[test]
    fn sid_roundtrip_regs_and_voices() {
        let mut m = Machine::new();
        m.sid_regs[0] = 0xaa;
        m.sid_regs[31] = 0x55;
        // Write a voice control byte through the engine so voice state changes.
        m.sid.write(0x04, 0x21, &m.sid_regs.clone()); // gate + sawtooth on V1
        let snap = capture_sid(&m);
        assert_eq!(snap.v, 2);
        assert_eq!(snap.regs.len(), 32);
        assert_eq!(snap.regs[0], 0xaa);
        assert_eq!(snap.voices.len(), 3);

        let mut m2 = Machine::new();
        restore_sid(&mut m2, &snap);
        assert_eq!(m2.sid_regs[0], 0xaa);
        assert_eq!(m2.sid_regs[31], 0x55);
        // voice fields round-trip
        let v0_orig = capture_sid(&m).voices[0].clone_via_serde();
        let v0_rest = capture_sid(&m2).voices[0].clone_via_serde();
        assert_eq!(v0_orig, v0_rest);
    }

    #[test]
    fn cpu_roundtrip() {
        let mut m = Machine::new();
        m.c64_core.reg_pc = 0xc123;
        m.c64_core.reg_a = 0x42;
        m.c64_core.reg_x = 0x10;
        m.c64_core.reg_y = 0x20;
        m.c64_core.reg_sp = 0xf0;
        m.c64_core.clk = 555_000;
        let flags = m.c64_core.status();

        let snap = capture_cpu(&m);
        assert_eq!(snap.pc, 0xc123);
        assert_eq!(snap.a, 0x42);
        assert_eq!(snap.flags, flags as i64);
        assert_eq!(snap.cycles, 555_000);

        let mut m2 = Machine::new();
        restore_cpu(&mut m2, &snap);
        assert_eq!(m2.c64_core.reg_pc, 0xc123);
        assert_eq!(m2.c64_core.reg_a, 0x42);
        assert_eq!(m2.cpu6510.reg_pc, 0xc123, "cpu6510 mirror");
        assert_eq!(m2.c64_core.clk, 555_000);
    }

    #[test]
    fn iec_roundtrip() {
        let mut m = Machine::new();
        m.iec.iecbus.cpu_bus = 0x55;
        m.iec.iecbus.cpu_port = 0xaa;
        m.iec.iecbus.drv_port = 0x85;
        m.iec.iec_old_atn = 0x10;
        m.iec.iecbus.drv_bus[8] = 0xc0;
        m.iec.iecbus.drv_data[8] = 0x10;

        let snap = capture_iec(&m);
        assert_eq!(snap.cpu_bus, 0x55);
        assert_eq!(snap.drv_bus.len(), 16);
        assert_eq!(snap.drv_bus[8], 0xc0);

        let mut m2 = Machine::new();
        restore_iec(&mut m2, &snap);
        assert_eq!(m2.iec.iecbus.cpu_bus, 0x55);
        assert_eq!(m2.iec.iecbus.cpu_port, 0xaa);
        assert_eq!(m2.iec.iec_old_atn, 0x10);
        assert_eq!(m2.iec.iecbus.drv_bus[8], 0xc0);
        assert_eq!(m2.iec.iecbus.drv_data[8], 0x10);
    }

    #[test]
    fn int_status_roundtrip_by_name() {
        let mut m = Machine::new();
        m.c64_int.pending_int = [0, 0x02, 0, 0]; // CIA1 IRQ asserted
        m.c64_int.nirq = 1;
        m.c64_int.irq_delay_cycles = 3;
        m.c64_int.global_pending_int = 0x42;

        let snap = capture_int_status(&m);
        assert_eq!(snap.int_names, vec!["vic-irq", "CIA1", "CIA2", "restore-nmi"]);
        assert_eq!(snap.pending_int, vec![0, 0x02, 0, 0]);
        assert_eq!(snap.nirq, 1);

        // Simulate a c64re dump with a DIFFERENT source order.
        let mut reordered = snap.clone();
        reordered.int_names = vec!["CIA1".into(), "vic-irq".into()];
        reordered.pending_int = vec![0x02, 0x00];
        let mut m2 = Machine::new();
        restore_int_status(&mut m2, &reordered);
        assert_eq!(m2.c64_int.pending_int[1], 0x02, "CIA1 lands in slot 1 by name");
        assert_eq!(m2.c64_int.pending_int[0], 0x00, "vic slot stays 0");
    }

    #[test]
    fn vic_roundtrip() {
        let mut m = Machine::new();
        m.vic.regs[0x11] = 0x1b;
        m.vic.regs[0x18] = 0x14;
        m.vic.raster_line = 137;
        m.vic.raster_cycle = 22;
        m.vic.raster_irq_line = 200;
        m.vic.vc = 0x123;
        m.vic.sprite[3].pointer = 0xab;
        m.vic.sprite[3].x = 0x1ff;
        m.vic.cregs[0x20] = 0x0e;
        m.vic.sbuf_reg[2] = 0xdeadbeef;
        m.vic.dbuf_line = 5;
        m.vic.dbuf[5 * crate::render::FB_W + 10] = 0x07;
        m.ram[0xd800] = 0x0a; // color RAM cell 0

        let snap = capture_vic(&m);
        assert_eq!(snap.regs.len(), 0x40);
        assert_eq!(snap.regs[0x11], 0x1b);
        assert_eq!(snap.raster_line, 137);
        assert_eq!(snap.vc, 0x123);
        assert_eq!(snap.sprite.len(), 8);
        assert_eq!(snap.sprite[3].pointer, 0xab);
        assert_eq!(snap.color_ram.len(), 0x400);
        assert_eq!(snap.color_ram[0], 0x0a);
        assert_eq!(snap.dbuf.len(), crate::render::FB_W as usize);
        assert_eq!(snap.dbuf[10], 0x07);

        let mut m2 = Machine::new();
        m2.vic.dbuf_line = 5; // restore writes the draw-line into the current row
        restore_vic(&mut m2, &snap);
        assert_eq!(m2.vic.regs[0x11], 0x1b);
        assert_eq!(m2.vic.raster_line, 137);
        assert_eq!(m2.vic.raster_cycle, 22);
        assert_eq!(m2.vic.vc, 0x123);
        assert_eq!(m2.vic.sprite[3].pointer, 0xab);
        assert_eq!(m2.vic.sprite[3].x, 0x1ff);
        assert_eq!(m2.vic.cregs[0x20], 0x0e);
        assert_eq!(m2.vic.sbuf_reg[2], 0xdeadbeef);
        assert_eq!(m2.ram[0xd800] & 0x0f, 0x0a);
        assert_eq!(m2.vic.dbuf[5 * crate::render::FB_W + 10], 0x07);
    }

    #[test]
    fn vic_presentation_roundtrip() {
        let mut m = Machine::new();
        m.vic.dbuf[100] = 0x05;
        m.vic.displayed[200] = 0x0b;
        m.vic.dbuf_line = 42;
        m.vic.ba_low_flag = true;
        m.vic.frame = 7;
        let snap = capture_vic_presentation(&m);

        let mut m2 = Machine::new();
        restore_vic_presentation(&mut m2, &snap);
        assert_eq!(m2.vic.dbuf[100], 0x05);
        assert_eq!(m2.vic.displayed[200], 0x0b);
        assert_eq!(m2.vic.dbuf_line, 42);
        assert!(m2.vic.ba_low_flag);
        assert_eq!(m2.vic.frame, 7);
    }

    #[test]
    fn ram_ta_roundtrip() {
        let mut m = Machine::new();
        m.ram[0x1000] = 0xde;
        m.ram[0xffff] = 0xad;
        let node = ram_ta(&m);
        let mut m2 = Machine::new();
        assert!(restore_ram_ta(&mut m2, &node));
        assert_eq!(m2.ram[0x1000], 0xde);
        assert_eq!(m2.ram[0xffff], 0xad);
    }

    /// Spec 792.1 — a banked EasyFlash cart's continuation (bank / register_02 /
    /// IO2 RAM) survives capture → restore. ROM-free (no boot): builds a synthetic
    /// EF CRT, drives the mapper into a non-zero bank, checkpoints, and restores into
    /// a fresh machine. Before the fix the restored cart resumed at bank 0.
    #[test]
    fn cart_state_roundtrip_easyflash_bank() {
        use crate::cart::BankInfo;

        // Minimal EF CRT (hw type 32) with two banks (distinct ROML fill).
        fn ef_crt() -> Vec<u8> {
            let mut v = Vec::new();
            v.extend_from_slice(b"C64 CARTRIDGE   ");
            v.extend_from_slice(&0x40u32.to_be_bytes());
            v.extend_from_slice(&0x0100u16.to_be_bytes());
            v.extend_from_slice(&32u16.to_be_bytes()); // EasyFlash
            v.push(1); // exrom
            v.push(0); // game
            v.extend_from_slice(&[0u8; 6]);
            v.extend_from_slice(&[0u8; 32]); // name
            for (bank, fill) in [(0u16, 0x11u8), (1u16, 0x22u8)] {
                v.extend_from_slice(b"CHIP");
                v.extend_from_slice(&(0x10u32 + 0x2000).to_be_bytes());
                v.extend_from_slice(&0u16.to_be_bytes());
                v.extend_from_slice(&bank.to_be_bytes());
                v.extend_from_slice(&0x8000u16.to_be_bytes());
                v.extend_from_slice(&0x2000u16.to_be_bytes());
                v.extend_from_slice(&vec![fill; 0x2000]);
            }
            v
        }
        let bi = BankInfo {
            cpu_port_direction: 0x2f,
            cpu_port_value: 0x37,
            basic_visible: true,
            kernal_visible: true,
            io_visible: true,
            char_visible: false,
            cartridge_attached: true,
            cartridge_exrom: None,
            cartridge_game: None,
            phi1: 0xff,
        };

        let crt = ef_crt();
        let mut m = Machine::new();
        m.attach_cart_from_bytes(&crt, "ef").expect("attach EF");
        let clk = m.c64_core.clk;
        if let Some(cart) = m.cartridge.as_mut() {
            cart.write(0xde02, 0x06, &bi, clk); // register_02 = 6 (8K mode)
            cart.write(0xde00, 0x01, &bi, clk); // bank 1
            cart.write(0xdf00, 0xa5, &bi, clk); // IO2 RAM
            cart.write(0xdf40, 0x5a, &bi, clk);
        }
        let want = m.cartridge.as_ref().unwrap().get_state();
        assert_eq!(want.current_bank, 1);
        assert_eq!(want.control_register, Some(0x06));

        // Capture the cart bytes + writable image the daemon would, then the tree.
        let cart_flash = m.cartridge.as_mut().and_then(|c| c.writable_image(clk));
        let cp = capture_runtime_checkpoint(&m, "", "", None, None, Some(&crt), cart_flash.as_deref());
        assert!(!cp["cartState"].is_null(), "cartState node emitted");
        assert_eq!(cp["cartState"]["currentBank"], 1);
        assert_eq!(cp["cartState"]["controlRegister"], 6);

        // Restore into a fresh machine (no prior cart).
        let mut m2 = Machine::new();
        restore_runtime_checkpoint(&mut m2, &cp).expect("restore");
        let got = m2.cartridge.as_ref().expect("cart re-attached").get_state();
        assert_eq!(got.current_bank, 1, "banked cart resumes at its captured bank");
        assert_eq!(got.control_register, Some(0x06), "register_02 restored");
        let f = got.flash.expect("EF flash state");
        assert_eq!(f.easyflash_ram[0x00], 0xa5, "IO2 RAM restored");
        assert_eq!(f.easyflash_ram[0x40], 0x5a, "IO2 RAM restored");
    }

    /// Spec 792.1 back-compat — a pre-792 `.c64re` (cart attached, but NO `cartState`
    /// node) still restores: the freshly-attached mapper stays at its boot state.
    #[test]
    fn cart_restore_without_cartstate_is_back_compat() {
        let mut m = Machine::new();
        // A checkpoint whose cart node exists but carries no cartState (707 shape).
        let crt = {
            let mut v = Vec::new();
            v.extend_from_slice(b"C64 CARTRIDGE   ");
            v.extend_from_slice(&0x40u32.to_be_bytes());
            v.extend_from_slice(&0x0100u16.to_be_bytes());
            v.extend_from_slice(&19u16.to_be_bytes()); // Magic Desk
            v.push(0);
            v.push(1);
            v.extend_from_slice(&[0u8; 6]);
            v.extend_from_slice(&[0u8; 32]);
            v.extend_from_slice(b"CHIP");
            v.extend_from_slice(&(0x10u32 + 0x2000).to_be_bytes());
            v.extend_from_slice(&0u16.to_be_bytes());
            v.extend_from_slice(&0u16.to_be_bytes());
            v.extend_from_slice(&0x8000u16.to_be_bytes());
            v.extend_from_slice(&0x2000u16.to_be_bytes());
            v.extend_from_slice(&vec![0x77u8; 0x2000]);
            v
        };
        let mut cp = capture_runtime_checkpoint(&m, "", "", None, None, Some(&crt), None);
        // Simulate a 707 dump: drop the additive cartState node entirely.
        cp.as_object_mut().unwrap().remove("cartState");
        assert!(cp.get("cartState").is_none());
        restore_runtime_checkpoint(&mut m, &cp).expect("restore 707-shape");
        // Cart re-attached at boot state (bank 0) — no panic, no partial restore.
        let st = m.cartridge.as_ref().expect("cart attached").get_state();
        assert_eq!(st.current_bank, 0);
    }
}

// Test-only helper: compare two voice snapshots structurally.
impl SidVoiceSnapshot {
    #[cfg(test)]
    fn clone_via_serde(&self) -> (i64, i64, i64, i64, i64) {
        (self.f, self.fs, self.adsrm, self.adsr_value, self.rv)
    }
}
