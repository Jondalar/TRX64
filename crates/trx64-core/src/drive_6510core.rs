//! VERBATIM port of the 1541 drive 6502 core.
//!
//! PORT OF (PRIMARY, 1:1):
//!   C64ReverseEngineeringMCP/src/runtime/headless/vice1541/drive_6510core.ts
//!   (2183 lines — the c64re drive CPU, itself a verbatim port of
//!    vice/src/6510core.c with #define DRIVE_CPU).
//! CROSS-CHECK: vice/src/6510core.c (the C original).
//!
//! This is a DEDICATED drive core, separate from the shared C64 cpu.rs. The
//! drive's rotate / byte-ready hooks are woven INTO the opcodes at the exact
//! cycle (BVC 0x50, BVS 0x70, PHP 0x08, CLV 0xb8 via LOCAL_SET_OVERFLOW(0)),
//! exactly as c64re/VICE do — NOT bolted on externally at the fetch boundary.
//!
//! Line-correspondence convention: every helper and opcode case is tagged with
//! `// ts:<N>` giving the drive_6510core.ts line number, and where the TS cites
//! a VICE line, `// 6510core.c:<N>`. A reviewer can diff this file against the
//! TS line-by-line by those tags.
//!
//! NOT wired into drive.rs yet — that is the next step. The interface is shaped
//! so a `drive_6510core_execute(&mut core, &mut bus)` running whole
//! instructions while `core.clk < stop_clk` fits drive.rs's run_cycles.

#![allow(clippy::needless_range_loop)]
#![allow(clippy::too_many_arguments)]
#![allow(non_snake_case)]
#![allow(clippy::collapsible_else_if)]

// =============================================================================
// SECTION A — Constants ported from vice/src/mos6510.h:52-59 (P_* flag bits)
// ts:88-99
// =============================================================================
const P_SIGN: u8 = 0x80;
const P_OVERFLOW: u8 = 0x40;
const P_UNUSED: u8 = 0x20;
const P_BREAK: u8 = 0x10;
const P_DECIMAL: u8 = 0x08;
const P_INTERRUPT: u8 = 0x04;
const P_ZERO: u8 = 0x02;
const P_CARRY: u8 = 0x01;

// PORT OF: vice/src/6510core.h:32-34 (opinfo masks). ts:101-104
const OPINFO_DELAYS_INTERRUPT_MSK: u32 = 1 << 8;
const OPINFO_DISABLES_IRQ_MSK: u32 = 1 << 9;
const OPINFO_ENABLES_IRQ_MSK: u32 = 1 << 10;

// PORT OF: vice/src/6510core.h:36-50 (opinfo accessors). ts:111-120
#[inline]
pub fn opinfo_delays_interrupt(opinfo: u32) -> u32 {
    opinfo & OPINFO_DELAYS_INTERRUPT_MSK
}
#[inline]
fn opinfo_disables_irq(opinfo: u32) -> u32 {
    opinfo & OPINFO_DISABLES_IRQ_MSK
}
#[inline]
pub fn opinfo_enables_irq(opinfo: u32) -> u32 {
    opinfo & OPINFO_ENABLES_IRQ_MSK
}

// PORT OF: vice/src/6510core.c:74-97 — CLK_* timing constants (non-DTV). ts:123-136
const CLK_RTS: u64 = 3;
const CLK_RTI: u64 = 4;
const CLK_BRK: u64 = 5;
const CLK_ABS_I_STORE2: u64 = 2;
const CLK_STACK_PUSH: u64 = 1;
const CLK_STACK_PULL: u64 = 2;
const CLK_ZERO_I_STORE: u64 = 2;
const CLK_ZERO_I2: u64 = 2;
const CLK_BRANCH2: u64 = 1;
const CLK_INT_CYCLE: u64 = 1;
const CLK_JSR_INT_CYCLE: u64 = 1;
const CLK_IND_Y_W: u64 = 2;
const CLK_NOOP_ZERO_X: u64 = 2;

// PORT OF: vice/src/6510core.c:95-98. ts:138-141
const NMI_CYCLES: u64 = 7;

// PORT OF: vice/src/interrupt.h:39-52. ts:143-152
pub const INTERRUPT_DELAY: u64 = 2;
pub const IK_NONE: u32 = 0;
pub const IK_NMI: u32 = 1 << 0;
pub const IK_IRQ: u32 = 1 << 1;
pub const IK_RESET: u32 = 1 << 2;
pub const IK_TRAP: u32 = 1 << 3;
pub const IK_MONITOR: u32 = 1 << 4;
pub const IK_IRQPEND: u32 = 1 << 6;

// PORT OF: vice/src/machine.h:188-192 — JAM reason codes. ts:154-158
pub const JAM_NONE: i32 = 0;
pub const JAM_RESET_CPU: i32 = 1;
pub const JAM_POWER_CYCLE: i32 = 2;
pub const JAM_MONITOR: i32 = 3;

// PORT OF: vice/src/traps.h — TRAP_OPCODE used by JAM_02(). ts:160-161
const TRAP_OPCODE: u8 = 0x02;

// PORT OF: vice/src/6510core.c:847-848 (ANE), :1389-1390 (LXA). ts:163-165
const ANE_MAGIC: u8 = 0xef;
const LXA_MAGIC: u8 = 0xee;

// =============================================================================
// SECTION B — interrupt state mirror.
//
// The TS reads these fields off interrupt_cpu_status_t via the `intf()` shim
// (ts:188-201, PORT OF vice/src/interrupt.h:55-129). Here they are a plain
// struct the host fills before each execute. ack helpers (ts:227-234,
// interrupt.c interrupt_ack_irq/_nmi/_reset) are methods on it.
// =============================================================================

/// "irrelevant" sentinel for an inactive irq_clk / nmi_clk / irq_pending_clk
/// (interrupt-cpu-status.ts:32 `CLOCK_MAX = Number.MAX_SAFE_INTEGER`; here the
/// u64 max). An interrupt is only honoured when `clk >= irq_clk + INTERRUPT_DELAY`,
/// so a MAX sentinel can never satisfy the check while no source asserts.
pub const CLOCK_MAX: u64 = u64::MAX;

/// Number of independent IRQ sources the drive's int status multiplexes
/// (interrupt.h pending_int[]): the 1541 wires VIA1 (int_num 0) and VIA2
/// (int_num 1) into the single CPU IRQ pin, so `nirq` counts both.
pub const DRIVE_NUM_INT_SOURCES: usize = 2;

/// Interrupt status mirror. PORT OF: vice/src/interrupt.h:55-129
/// (interrupt_cpu_status_s subset used by 6510core.c). ts:188-195
#[derive(Clone, Debug)]
pub struct IntStatus {
    pub irq_clk: u64,
    pub nmi_clk: u64,
    pub irq_pending_clk: u64,
    pub global_pending_int: u32,
    /// interrupt.h:111 last_opcode_info_ptr target. Kept in sync with
    /// DriveCore6510.last_opcode_info by SET_LAST_OPCODE et al. (ts:530-538).
    pub last_opcode_info_ptr: u32,
    pub nnmi: u32,
    /// interrupt.h:74-80 — per-source asserted-line bitmask (pending_int[]).
    /// Index = int_num (0=VIA1, 1=VIA2). Tracks IK_IRQ per source so `nirq`
    /// counts edges correctly when two sources overlap.
    pub pending_int: [u32; DRIVE_NUM_INT_SOURCES],
    /// interrupt.h `nirq` — how many sources currently assert IRQ. `irq_clk` is
    /// stamped only on the 0→1 transition (interrupt-cpu-status.ts:116-127).
    pub nirq: u32,
}

impl Default for IntStatus {
    fn default() -> Self {
        Self::new()
    }
}

impl IntStatus {
    /// Power-on / reset state: clocks at the CLOCK_MAX "inactive" sentinel, no
    /// pending interrupts (interrupt_cpu_status_reset, interrupt.c).
    pub fn new() -> Self {
        IntStatus {
            irq_clk: CLOCK_MAX,
            nmi_clk: CLOCK_MAX,
            irq_pending_clk: CLOCK_MAX,
            global_pending_int: IK_NONE,
            last_opcode_info_ptr: 0,
            nnmi: 0,
            pending_int: [IK_NONE; DRIVE_NUM_INT_SOURCES],
            nirq: 0,
        }
    }

    /// PORT OF: vice/src/interrupt.h:141-196 (interrupt_set_irq) — the per-source
    /// IRQ-line setter the VIA `set_int` calls (via update_myviairq_rclk) with the
    /// precise underflow/level rclk. `int_num` selects the source (0=VIA1, 1=VIA2);
    /// `value` is the new level; `rclk` is the stamp clock. Mirrors
    /// interrupt-cpu-status.ts:109-140 exactly: `irq_clk` is set ONLY on the
    /// `nirq` 0→1 edge (first source to assert), and on the final deassert
    /// `irq_pending_clk = rclk + 3` arms the IK_IRQPEND tail. The drive has no DMA
    /// cycle-stealing (`last_stolen_cycles_clk <= rclk` always holds), so the
    /// `fixup_int_clk` branch never fires.
    #[inline]
    pub fn set_irq(&mut self, int_num: usize, value: bool, rclk: u64) {
        if int_num >= self.pending_int.len() {
            return;
        }
        if value {
            if self.pending_int[int_num] & IK_IRQ == 0 {
                self.pending_int[int_num] |= IK_IRQ;
                if self.nirq == 0 {
                    self.global_pending_int |= IK_IRQ | IK_IRQPEND;
                    self.irq_pending_clk = CLOCK_MAX;
                    self.irq_clk = rclk;
                }
                self.nirq += 1;
            }
        } else if self.pending_int[int_num] & IK_IRQ != 0 {
            if self.nirq > 0 {
                self.pending_int[int_num] &= !IK_IRQ;
                self.nirq -= 1;
                if self.nirq == 0 {
                    self.global_pending_int &= !IK_IRQ;
                    self.irq_pending_clk = rclk + 3;
                }
            }
        }
    }

    /// PORT OF: vice/src/interrupt.h:284-289 interrupt_ack_irq.
    ///
    /// VICE clears ONLY `IK_IRQPEND` here (the one-shot "an IRQ dispatch is due"
    /// latch) and parks `irq_pending_clk` at the inactive sentinel. It does NOT
    /// clear `IK_IRQ`: that bit is the IRQ *level*, which tracks `nirq > 0` and is
    /// cleared solely by `interrupt_set_irq` when the LAST source deasserts
    /// (`--nirq == 0`). Clearing `IK_IRQ` here was the bug: after the first drive
    /// IRQ was acknowledged while a second source stayed asserted (`nirq` stuck at
    /// 2 — e.g. the VIA2 T1 watchdog still pending when the VIA1 CA1 ATN edge
    /// arrives), `global_pending_int` permanently lost `IK_IRQ` (it only re-arms on
    /// the `nirq` 0→1 edge, which never recurs), so the prologue's
    /// `global_pending_int != IK_NONE` test went false and the ATN-service IRQ was
    /// never dispatched — the drive sat in its $EC12 idle loop while the C64 held
    /// ATN at $EEAC. Matching VICE (clear IK_IRQPEND only) restores dispatch.
    #[inline]
    pub fn interrupt_ack_irq(&mut self) {
        self.global_pending_int &= !IK_IRQPEND;
        self.irq_pending_clk = CLOCK_MAX;
    }
    /// PORT OF: vice/src/interrupt.c interrupt_ack_nmi. ts:229
    #[inline]
    pub fn interrupt_ack_nmi(&mut self) {
        self.global_pending_int &= !IK_NMI;
        self.nnmi += 1;
    }
    /// PORT OF: vice/src/interrupt.c interrupt_ack_reset. ts:230
    #[inline]
    pub fn interrupt_ack_reset(&mut self) {
        self.global_pending_int &= !IK_RESET;
    }
}

// =============================================================================
// SECTION C — interrupt_check_{nmi,irq}_delay.
//
// In VICE these are inline-static in drivecpu.c (lines 303/329), defined BEFORE
// `#include "6510core.c"` so DO_INTERRUPT sees them. The TS imports them back
// from drivecpu.ts (ts:246-249). Here they are free functions taking the
// IntStatus by ref + the clk. PORT OF: vice/src/drive/drivecpu.c:303-355.
// =============================================================================

/// PORT OF: vice/src/drive/drivecpu.h:34-38 — OPINFO_NUMBER(opinfo) = low byte
/// (the opcode of the last-executed instruction). drivetypes.ts:328-334.
const OPINFO_NUMBER_MSK: u32 = 0xff;
#[inline]
fn opinfo_number(opinfo: u32) -> u32 {
    opinfo & OPINFO_NUMBER_MSK
}

/// PORT OF: vice/src/drive/drivecpu.c:327-351 (interrupt_check_irq_delay, inline
/// static — drivecpu.ts:1224-1243). NOT the simplified window check: the full
/// VICE logic honours the per-opcode interrupt-latency modifiers that make the
/// drive watchdog IRQ enter cycle-for-cycle with VICE.
///   1. `irq_clk = f.irq_clk + INTERRUPT_DELAY`.
///   2. A taken-no-page-cross branch (OPINFO_DELAYS_INTERRUPT, set by BRANCH at
///      6510core.c:991) delays the IRQ by ONE extra cycle → `irq_clk++`.
///   3. If `cpu_clk >= irq_clk`: take the IRQ UNLESS the last opcode ENABLES_IRQ
///      (an I-clearing CLI/PLP/RTI — OPINFO_ENABLES_IRQ), in which case the IRQ is
///      deferred a FULL instruction by latching `IK_IRQPEND` (the CPU runs one more
///      opcode before honouring the IRQ). This mutates `int_status`.
/// Returns true iff the IRQ should be dispatched on THIS instruction.
#[inline]
fn interrupt_check_irq_delay(int_status: &mut IntStatus, clk: u64) -> bool {
    let mut irq_clk = int_status.irq_clk + INTERRUPT_DELAY;
    if opinfo_delays_interrupt(int_status.last_opcode_info_ptr) != 0 {
        irq_clk += 1;
    }
    if clk >= irq_clk {
        if opinfo_enables_irq(int_status.last_opcode_info_ptr) == 0 {
            return true;
        } else {
            int_status.global_pending_int |= IK_IRQPEND;
        }
    }
    false
}

/// PORT OF: vice/src/drive/drivecpu.c:303-325 (interrupt_check_nmi_delay, inline
/// static — drivecpu.ts:1197-1219). The full VICE logic:
///   1. `nmi_clk = f.nmi_clk + INTERRUPT_DELAY`.
///   2. BRK (opcode 0x00) defers the NMI by one opcode → return 0.
///   3. A taken-no-page-cross branch (OPINFO_DELAYS_INTERRUPT) delays it one cycle.
///   4. Take the NMI iff `cpu_clk >= nmi_clk`.
#[inline]
fn interrupt_check_nmi_delay(int_status: &IntStatus, clk: u64) -> bool {
    let mut nmi_clk = int_status.nmi_clk + INTERRUPT_DELAY;
    if opinfo_number(int_status.last_opcode_info_ptr) == 0x00 {
        return false;
    }
    if opinfo_delays_interrupt(int_status.last_opcode_info_ptr) != 0 {
        nmi_clk += 1;
    }
    clk >= nmi_clk
}

// =============================================================================
// SECTION D/E/F/J — host hooks via the bus trait.
//
// The TS uses module-level installable slots for drivecpu_rotate /
// drivecpu_byte_ready / drivecpu_byte_ready_egde_clear (ts:260-275, PORT OF
// drivecpu.c:423-433), drive_trap_handler (ts:289-295, drivecpu.c:272-290),
// the debug trace hook (ts:302-308), cpu_reset (ts:2152-2158, drivecpu.c:165-
// 184) and drivecpu_jam (ts:2153-2163, drivecpu.c:485-539). Here those become
// methods on the DriveCore6510Bus trait so drive.rs can implement them over its
// existing DriveBus + rotation — no global mutable state.
// =============================================================================

/// Bus + rotation + host-hook surface the drive core executes against.
///
/// drive.rs implements this over its existing DriveBus (RAM/ROM/VIA) and
/// rotation model. Real vs DUMMY accesses are distinct methods because the TS
/// routes them through separate `read_func_ptr` / `read_func_ptr_dummy` tables
/// (ts:390-430) — the dummy variants exist so checkpoints / side-effects can
/// distinguish a true access from a wasted bus cycle. Default dummy impls
/// delegate to the real ones (matching a plain RAM bus with no checkpoints).
pub trait DriveCore6510Bus {
    /// PORT OF: drivecpu.c:131-143 read_func_ptr. ts:390-394
    fn read(&mut self, addr: u16) -> u8;
    /// PORT OF: drivecpu.c:131-143 store_func_ptr. ts:406-410
    fn write(&mut self, addr: u16, value: u8);

    /// DUMMY read (read_func_ptr_dummy). Default = real read. ts:416-420
    #[inline]
    fn read_dummy(&mut self, addr: u16) -> u8 {
        self.read(addr)
    }
    /// DUMMY write (store_func_ptr_dummy). Default = real write. ts:426-430
    #[inline]
    fn write_dummy(&mut self, addr: u16, value: u8) {
        self.write(addr, value)
    }

    /// PORT OF: drivecpu.c:423-433 drivecpu_rotate. ts:260/267-275
    fn rotate(&mut self);
    /// PORT OF: drivecpu.c:423-433 drivecpu_byte_ready. ts:261
    fn byte_ready(&mut self) -> bool;
    /// PORT OF: drivecpu.c:423-433 drivecpu_byte_ready_egde_clear (sic). ts:262
    fn byte_ready_edge_clear(&mut self);

    /// PROCESS_ALARMS hook (6510core.c:139-146). Advances the drive's VIA/alarm
    /// machinery up to `clk`. The TS calls the alarm context dispatch loop +
    /// caller alarm_dispatch (ts:566-581). drive.rs ticks its VIAs here.
    #[inline]
    fn process_alarms(&mut self, _clk: u64) {}

    /// PORT OF: drivecpu.c:272-290 drive_trap_handler. Returns 0 if PC matched
    /// unit->trap (handled), or 0xffffffff (= (uint32_t)-1) otherwise → JAM.
    /// Default 0xffffffff (no trap installed → JAM_02 jams). ts:289-295
    #[inline]
    fn trap_handler(&mut self) -> u32 {
        0xffff_ffff
    }

    /// PORT OF: drivecpu.c:485-539 drivecpu_jam. Returns JAM_NONE /
    /// JAM_RESET_CPU / JAM_POWER_CYCLE / JAM_MONITOR. Default: bump CLK by the
    /// `default:` path is handled by the caller; here None means "no host jam
    /// handler" → caller adds 1 cycle (ts:1117-1123). ts:2159-2163
    #[inline]
    fn jam(&mut self) -> Option<i32> {
        None
    }

    /// PORT OF: drivecpu.c:165-184 cpu_reset. Called on IK_RESET dispatch.
    /// ts:2152-2158 / 2167-2177
    #[inline]
    fn cpu_reset(&mut self) {}

    /// PORT OF: 6510core.c:2415-2427 debug_drive trace branch. Default no-op.
    /// ts:302-308 / 1724
    #[inline]
    fn debug_drive(&mut self, _pc: u16, _clk: u64, _op: u8, _p1: u8, _p2hi: u8) {}
}

// =============================================================================
// SECTION (registers) — DriveCore6510 struct.
//
// Mirrors the TS register locals (ts:344-369) that live in drv->cpu->cpu_regs +
// drv->cpu->d_bank_*. Field names follow the TS where possible: flag_n / flag_z
// are split shadow vars (ts:360-364, 6510core.c:210-222); reg_p has P_ZERO +
// P_SIGN masked out (ts:358). The bank fast-path cache is d_bank_* (ts:366-369,
// drivecpu.c:436-438). is_jammed + last_opcode_info + last_opcode_addr live
// here (drv->cpu fields).
// =============================================================================

/// Drive 6502 register + execution state. PORT OF: drv->cpu->cpu_regs +
/// drv->cpu->d_bank_* + is_jammed/last_opcode_* (drivecpu_context_t).
#[derive(Clone, Debug)]
pub struct DriveCore6510 {
    pub reg_a: u8,
    pub reg_x: u8,
    pub reg_y: u8,
    pub reg_sp: u8,
    /// P with P_ZERO + P_SIGN masked OUT — flag_n/flag_z are authoritative for
    /// those. ts:358 / 6510core.c:212.
    pub reg_p: u8,
    pub reg_pc: u16,
    /// 0x80 if N set, else 0 (VICE flag_n cache). ts:363.
    pub flag_n: u8,
    /// 0 iff Z set; non-zero iff Z clear (VICE flag_z cache). ts:364.
    pub flag_z: u8,

    /// Master clock, mirrors `*clk_ptr` advanced by CLK_ADD. ts:337/378.
    pub clk: u64,

    /// Bank fast-path cache (drivecpu.c:436-438). `d_bank_base` is a 64 KB
    /// window pointer in C; here it is an optional 64 KB RAM mirror the host
    /// pins when the page has a flat base. None ⇒ per-byte LOAD path. ts:366-369.
    pub d_bank_base: Option<Box<[u8; 0x10000]>>,
    pub d_bank_start: u16,
    pub d_bank_limit: u16,

    /// drv->cpu->last_opcode_info — OPINFO word. ts:536.
    pub last_opcode_info: u32,
    /// drv->cpu->last_opcode_addr. ts:556.
    pub last_opcode_addr: u16,
    /// drv->cpu->is_jammed. ts:1094.
    pub is_jammed: bool,
}

impl Default for DriveCore6510 {
    fn default() -> Self {
        Self::new()
    }
}

impl DriveCore6510 {
    /// Power-on state. The drive 6502 powers on with SP=0 (drivecpu init);
    /// flags = P_UNUSED. d_bank_* zero ⇒ per-byte LOAD path until a JUMP caches
    /// a bank.
    pub fn new() -> Self {
        DriveCore6510 {
            reg_a: 0,
            reg_x: 0,
            reg_y: 0,
            reg_sp: 0,
            reg_p: P_UNUSED & !(P_ZERO | P_SIGN),
            reg_pc: 0,
            flag_n: 0,
            flag_z: 1, // Z clear at power-on (flag_z != 0).
            clk: 0,
            d_bank_base: None,
            d_bank_start: 0,
            d_bank_limit: 0,
            last_opcode_info: 0,
            last_opcode_addr: 0,
            is_jammed: false,
        }
    }

    /// Composite P register (incl. flag_n/flag_z view). = LOCAL_STATUS() saved
    /// into regs.flags at the end of the body (ts:2042 / 524-526).
    #[inline]
    pub fn status(&self) -> u8 {
        self.reg_p | (self.flag_n & P_SIGN) | P_UNUSED | (if self.flag_z == 0 { P_ZERO } else { 0 })
    }

    /// Load a composite P (e.g. from a snapshot): split flag_n/flag_z back out.
    /// = the import at ts:358-364.
    #[inline]
    pub fn set_status_composite(&mut self, v: u8) {
        self.reg_p = v & !(P_ZERO | P_SIGN);
        self.flag_n = if v & P_SIGN != 0 { 0x80 } else { 0 };
        self.flag_z = if v & P_ZERO != 0 { 0 } else { 1 };
    }
}

// =============================================================================
// SECTION fetch_tab (6510core.c:2016-2034) — module-level static. ts:2123-2145
// =============================================================================
// PORT OF: vice/src/6510core.c:2016-2034 (fetch_tab) — 1 when opcode is 3 bytes.
#[rustfmt::skip]
const FETCH_TAB: [u8; 256] = [
    /* $00 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1,
    /* $10 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 1, 1, 1,
    /* $20 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1,
    /* $30 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 1, 1, 1,
    /* $40 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1,
    /* $50 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 1, 1, 1,
    /* $60 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1,
    /* $70 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 1, 1, 1,
    /* $80 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1,
    /* $90 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 1, 1, 1,
    /* $A0 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1,
    /* $B0 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 1, 1, 1,
    /* $C0 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1,
    /* $D0 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 1, 1, 1,
    /* $E0 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1,
    /* $F0 */  0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 1, 1, 1,
];

// =============================================================================
// SECTION G — execution context.
//
// The TS body is one big closure over `reg_*` locals + `clk_ptr` + `drv`. In
// Rust we bundle them into a per-call `Exec` borrowing the core, the bus, and
// the int status. Every TS helper (CLK_ADD, LOAD_*, the ALU op fns, the opcode
// switch) becomes a method on Exec so it can mutate reg_* / clk and call the
// bus. Method names match the VICE macro names verbatim (snake/upper) so grep
// maps 1:1 against 6510core.c. ts:331-334.
// =============================================================================

struct Exec<'a, B: DriveCore6510Bus> {
    core: &'a mut DriveCore6510,
    bus: &'a mut B,
    int: &'a mut IntStatus,
    /// Result of any JAM dispatch this step (ts:372).
    jam_result: i32,
}

/// Function-pointer type for the load/store/dummy closures the opcode switch
/// passes into the RMW op helpers (ASL/ROL/DEC/...). In C these are macro
/// names; the TS passes them as arrow fns (ts:830-832). In Rust we dispatch on
/// a small enum to stay object-safe + avoid borrow gymnastics.
#[derive(Clone, Copy)]
enum LoadFn {
    Abs,
    Zero,
    AbsXRmw,
    AbsYRmw,
}
#[derive(Clone, Copy)]
enum StoreFn {
    Abs,
    AbsX,
    AbsY,
    AbsXRmw,
    AbsYRmw,
}
#[derive(Clone, Copy)]
enum DummyFn {
    AbsRmw,
    AbsXRmw,
    AbsYRmw,
}

impl<'a, B: DriveCore6510Bus> Exec<'a, B> {
    // -------------------------------------------------------------------------
    // CLK helpers (CLK_ADD macro family from 6510core.c:114-119). ts:377-385
    // -------------------------------------------------------------------------
    #[inline]
    fn clk_add(&mut self, n: i64) {
        // wrapping signed add (BVC/BVS pass -1). ts:378.
        self.core.clk = (self.core.clk as i64).wrapping_add(n) as u64;
    }
    #[inline]
    fn clk_add_dummy(&mut self, n: i64) {
        self.clk_add(n);
    }
    #[inline]
    fn rewind_fetch_opcode(&mut self) {
        self.core.clk = self.core.clk.wrapping_sub(2);
    }

    // -------------------------------------------------------------------------
    // Memory access (drivecpu.c:131-143). ts:390-438
    // -------------------------------------------------------------------------
    #[inline]
    fn load(&mut self, a: u16) -> u8 {
        self.bus.read(a)
    }
    #[inline]
    fn load_zero(&mut self, a: u8) -> u8 {
        self.bus.read(a as u16)
    }
    #[inline]
    fn load_addr(&mut self, a: u16) -> u16 {
        let lo = self.load(a) as u16;
        let hi = self.load(a.wrapping_add(1)) as u16;
        lo | (hi << 8)
    }
    #[inline]
    fn load_zero_addr(&mut self, a: u8) -> u16 {
        let lo = self.load_zero(a) as u16;
        let hi = self.load_zero(a.wrapping_add(1)) as u16;
        lo | (hi << 8)
    }
    #[inline]
    fn store(&mut self, a: u16, b: u8) {
        self.bus.write(a, b);
    }
    #[inline]
    fn store_zero(&mut self, a: u8, b: u8) {
        self.bus.write(a as u16, b);
    }
    #[inline]
    fn load_dummy(&mut self, a: u16) -> u8 {
        self.bus.read_dummy(a)
    }
    #[inline]
    fn load_zero_dummy(&mut self, a: u8) -> u8 {
        self.bus.read_dummy(a as u16)
    }
    #[inline]
    fn store_dummy(&mut self, a: u16, b: u8) {
        self.bus.write_dummy(a, b);
    }

    // FETCH_PARAM = LOAD (DRIVE_CPU path; 6510core.c:537-542). ts:432-438
    #[inline]
    fn fetch_param(&mut self, a: u16) -> u8 {
        self.load(a)
    }
    #[inline]
    fn fetch_param_dummy(&mut self, a: u16) -> u8 {
        self.load_dummy(a)
    }

    // LOAD_IND / STORE_IND collapse to LOAD/STORE (non-6509). ts:440-443
    #[inline]
    fn store_ind(&mut self, a: u16, b: u8) {
        self.store(a, b);
    }

    // -------------------------------------------------------------------------
    // Stack ops (6510core.c:370-376). ts:448-455
    // -------------------------------------------------------------------------
    #[inline]
    fn push(&mut self, val: u8) {
        self.store(0x100 + self.core.reg_sp as u16, val);
        self.core.reg_sp = self.core.reg_sp.wrapping_sub(1);
    }
    #[inline]
    fn pull(&mut self) -> u8 {
        self.core.reg_sp = self.core.reg_sp.wrapping_add(1);
        self.load(0x100 + self.core.reg_sp as u16)
    }

    // -------------------------------------------------------------------------
    // JUMP (drivecpu.c:145-161). Updates the cached bank fast-path. ts:461-475
    // -------------------------------------------------------------------------
    #[inline]
    fn jump(&mut self, addr: u16) {
        self.core.reg_pc = addr;
        if self.core.reg_pc >= self.core.d_bank_limit || self.core.reg_pc < self.core.d_bank_start {
            // VICE caches base/limit from read_base_tab_ptr / read_limit_tab_ptr.
            // Our bus exposes that through the trait's flat-window only when the
            // host pinned d_bank_base. Without a base table accessor we keep the
            // cache window as-is once pinned; an out-of-window JUMP just clears
            // the start/limit so subsequent fetches take the per-byte LOAD path.
            // (drivemem leaves base_tab_ptr NULL for most pages — ts:67-71.)
            if self.core.d_bank_base.is_none() {
                self.core.d_bank_start = 0;
                self.core.d_bank_limit = 0;
            }
        }
    }

    // -------------------------------------------------------------------------
    // Flag helpers (6510core.c:150-224). ts:480-526
    // -------------------------------------------------------------------------
    #[inline]
    fn local_set_nz(&mut self, val: u8) {
        self.core.flag_z = val;
        self.core.flag_n = val;
    }
    /// PORT OF: vice/src/6510core.c:152-162 — DRIVE_CPU LOCAL_SET_OVERFLOW that
    /// performs drivecpu_rotate + byte_ready_edge_clear when val is 0. ts:486-494
    #[inline]
    fn local_set_overflow(&mut self, val: bool) {
        if val {
            self.core.reg_p |= P_OVERFLOW;
        } else {
            self.bus.rotate();
            self.bus.byte_ready_edge_clear();
            self.core.reg_p &= !P_OVERFLOW;
        }
    }
    #[inline]
    fn local_set_break(&mut self, val: bool) {
        if val {
            self.core.reg_p |= P_BREAK;
        } else {
            self.core.reg_p &= !P_BREAK;
        }
    }
    #[inline]
    fn local_set_decimal(&mut self, val: bool) {
        if val {
            self.core.reg_p |= P_DECIMAL;
        } else {
            self.core.reg_p &= !P_DECIMAL;
        }
    }
    #[inline]
    fn local_set_interrupt(&mut self, val: bool) {
        if val {
            self.core.reg_p |= P_INTERRUPT;
        } else {
            self.core.reg_p &= !P_INTERRUPT;
        }
    }
    #[inline]
    fn local_set_carry(&mut self, val: bool) {
        if val {
            self.core.reg_p |= P_CARRY;
        } else {
            self.core.reg_p &= !P_CARRY;
        }
    }
    #[inline]
    fn local_set_sign(&mut self, val: bool) {
        self.core.flag_n = if val { 0x80 } else { 0 };
    }
    #[inline]
    fn local_set_zero(&mut self, val: bool) {
        self.core.flag_z = if val { 0 } else { 1 };
    }
    #[inline]
    fn local_set_status(&mut self, val: u8) {
        self.core.reg_p = val & !(P_ZERO | P_SIGN);
        self.local_set_zero(val & P_ZERO != 0);
        self.core.flag_n = val;
    }
    #[inline]
    fn local_overflow(&self) -> bool {
        self.core.reg_p & P_OVERFLOW != 0
    }
    #[inline]
    fn local_decimal(&self) -> bool {
        self.core.reg_p & P_DECIMAL != 0
    }
    #[inline]
    fn local_interrupt(&self) -> bool {
        self.core.reg_p & P_INTERRUPT != 0
    }
    #[inline]
    fn local_carry(&self) -> bool {
        self.core.reg_p & P_CARRY != 0
    }
    #[inline]
    fn local_sign(&self) -> bool {
        self.core.flag_n & 0x80 != 0
    }
    #[inline]
    fn local_zero(&self) -> bool {
        self.core.flag_z == 0
    }
    #[inline]
    fn local_status(&self) -> u8 {
        self.core.reg_p
            | (self.core.flag_n & 0x80)
            | P_UNUSED
            | (if self.local_zero() { P_ZERO } else { 0 })
    }

    // -------------------------------------------------------------------------
    // Last-opcode-info bookkeeping (6510core.c:226-254). ts:535-557
    // The interrupt status's last_opcode_info_ptr aliases the same word.
    // -------------------------------------------------------------------------
    #[inline]
    fn set_last_opcode(&mut self, x: u32) {
        self.core.last_opcode_info = x & 0xff; // OPINFO_SET clears delays/disables/enables.
        self.int.last_opcode_info_ptr = self.core.last_opcode_info;
    }
    #[inline]
    fn opcode_delays_interrupt(&mut self) {
        self.core.last_opcode_info |= OPINFO_DELAYS_INTERRUPT_MSK;
        self.int.last_opcode_info_ptr = self.core.last_opcode_info;
    }
    #[inline]
    fn opcode_disables_irq(&mut self) {
        self.core.last_opcode_info |= OPINFO_DISABLES_IRQ_MSK;
        self.int.last_opcode_info_ptr = self.core.last_opcode_info;
    }
    #[inline]
    fn opcode_enables_irq(&mut self) {
        self.core.last_opcode_info |= OPINFO_ENABLES_IRQ_MSK;
        self.int.last_opcode_info_ptr = self.core.last_opcode_info;
    }
    #[inline]
    fn set_last_addr(&mut self, x: u16) {
        self.core.last_opcode_addr = x;
    }

    // -------------------------------------------------------------------------
    // Alarm processing (6510core.c:139-146 PROCESS_ALARMS). ts:566-581
    // -------------------------------------------------------------------------
    #[inline]
    fn process_alarms(&mut self) {
        let clk = self.core.clk;
        self.bus.process_alarms(clk);
    }

    // -------------------------------------------------------------------------
    // Addressing helpers (6510core.c:547-711, DRIVE_CPU). ts:586-733
    // -------------------------------------------------------------------------
    #[inline]
    fn load_abs(&mut self, a: u16) -> u8 {
        self.load(a)
    }

    // PORT OF: vice/src/6510core.c:549-554 (LOAD_ABS_X). ts:589-596
    fn load_abs_x(&mut self, addr: u16) -> u8 {
        if ((addr & 0xff) as u32 + self.core.reg_x as u32) > 0xff {
            let da = (addr & 0xff00) | (addr.wrapping_add(self.core.reg_x as u16) & 0xff);
            self.load_dummy(da);
            self.clk_add(CLK_INT_CYCLE as i64);
            return self.load(addr.wrapping_add(self.core.reg_x as u16));
        }
        self.load(addr.wrapping_add(self.core.reg_x as u16))
    }
    // PORT OF: vice/src/6510core.c:556-561 (NOOP_LOAD_ABS_X). ts:598-605
    fn noop_load_abs_x(&mut self, addr: u16) -> u8 {
        if ((addr & 0xff) as u32 + self.core.reg_x as u32) > 0xff {
            let da = (addr & 0xff00) | (addr.wrapping_add(self.core.reg_x as u16) & 0xff);
            self.load_dummy(da);
            self.clk_add(CLK_INT_CYCLE as i64);
            return self.load_dummy(addr.wrapping_add(self.core.reg_x as u16));
        }
        self.load_dummy(addr.wrapping_add(self.core.reg_x as u16))
    }
    // PORT OF: vice/src/6510core.c:563-566 (LOAD_ABS_X_RMW). ts:607-611
    fn load_abs_x_rmw(&mut self, addr: u16) -> u8 {
        let da = (addr & 0xff00) | (addr.wrapping_add(self.core.reg_x as u16) & 0xff);
        self.load_dummy(da);
        self.clk_add(CLK_INT_CYCLE as i64);
        self.load(addr.wrapping_add(self.core.reg_x as u16))
    }
    // PORT OF: vice/src/6510core.c:568-573 (LOAD_ABS_Y). ts:613-620
    fn load_abs_y(&mut self, addr: u16) -> u8 {
        if ((addr & 0xff) as u32 + self.core.reg_y as u32) > 0xff {
            let da = (addr & 0xff00) | (addr.wrapping_add(self.core.reg_y as u16) & 0xff);
            self.load_dummy(da);
            self.clk_add(CLK_INT_CYCLE as i64);
            return self.load(addr.wrapping_add(self.core.reg_y as u16));
        }
        self.load(addr.wrapping_add(self.core.reg_y as u16))
    }
    // PORT OF: vice/src/6510core.c:575-578 (LOAD_ABS_Y_RMW). ts:622-626
    fn load_abs_y_rmw(&mut self, addr: u16) -> u8 {
        let da = (addr & 0xff00) | (addr.wrapping_add(self.core.reg_y as u16) & 0xff);
        self.load_dummy(da);
        self.clk_add(CLK_INT_CYCLE as i64);
        self.load(addr.wrapping_add(self.core.reg_y as u16))
    }
    // PORT OF: vice/src/6510core.c:583-589 (LOAD_IND_X). ts:628-634
    fn load_ind_x(&mut self, addr: u8) -> u8 {
        self.clk_add(3);
        self.load_zero_dummy(addr);
        let mut tmpa = self.load_zero(addr.wrapping_add(self.core.reg_x)) as u16;
        tmpa |= (self.load_zero(addr.wrapping_add(self.core.reg_x).wrapping_add(1)) as u16) << 8;
        self.load(tmpa)
    }
    // PORT OF: vice/src/6510core.c:601-609 (LOAD_IND_Y). ts:636-646
    fn load_ind_y(&mut self, addr: u8) -> u8 {
        self.clk_add(2);
        let mut tmpa = self.load_zero(addr) as u16;
        tmpa |= (self.load_zero(addr.wrapping_add(1)) as u16) << 8;
        if ((tmpa & 0xff) as u32 + self.core.reg_y as u32) > 0xff {
            self.clk_add(CLK_INT_CYCLE as i64);
            let da = (tmpa & 0xff00) | (tmpa.wrapping_add(self.core.reg_y as u16) & 0xff);
            self.load_dummy(da);
            return self.load(tmpa.wrapping_add(self.core.reg_y as u16));
        }
        self.load(tmpa.wrapping_add(self.core.reg_y as u16))
    }
    // PORT OF: vice/src/6510core.c:640-648 (LOAD_IND_Y_BANK) → LOAD_IND_Y. ts:648-650
    fn load_ind_y_bank(&mut self, addr: u8) -> u8 {
        self.load_ind_y(addr)
    }
    // PORT OF: vice/src/6510core.c:618-620 (LOAD_ZERO_X). ts:652-655
    fn load_zero_x(&mut self, addr: u8) -> u8 {
        self.load_zero_dummy(addr);
        self.load_zero(addr.wrapping_add(self.core.reg_x))
    }
    // PORT OF: vice/src/6510core.c:622-624 (NOOP_LOAD_ZERO_X). ts:657-660
    fn noop_load_zero_x(&mut self, addr: u8) {
        self.load_zero_dummy(addr);
        self.load_zero_dummy(addr.wrapping_add(self.core.reg_x));
    }
    // PORT OF: vice/src/6510core.c:626-628 (LOAD_ZERO_Y). ts:662-665
    fn load_zero_y(&mut self, addr: u8) -> u8 {
        self.load_zero_dummy(addr);
        self.load_zero(addr.wrapping_add(self.core.reg_y))
    }

    // STORE_ABS family (6510core.c:651-711). ts:667-718
    // PORT OF: vice/src/6510core.c:651-655 (STORE_ABS). ts:669-672
    fn store_abs(&mut self, addr: u16, value: u8, inc: i64) {
        self.clk_add(inc);
        self.store(addr, value);
    }
    // PORT OF: vice/src/6510core.c:657-663 (STORE_ABS_X). ts:674-679
    fn store_abs_x(&mut self, addr: u16, value: u8, inc: i64) {
        self.clk_add(inc - 2);
        let da = (addr.wrapping_add(self.core.reg_x as u16) & 0xff) | (addr & 0xff00);
        self.load_dummy(da);
        self.clk_add(2);
        self.store(addr.wrapping_add(self.core.reg_x as u16), value);
    }
    // PORT OF: vice/src/6510core.c:665-669 (STORE_ABS_X_RMW). ts:681-684
    fn store_abs_x_rmw(&mut self, addr: u16, value: u8, inc: i64) {
        self.clk_add(inc);
        self.store(addr.wrapping_add(self.core.reg_x as u16), value);
    }
    // PORT OF: vice/src/6510core.c:671-683 (STORE_ABS_SH_X). ts:686-695
    fn store_abs_sh_x(&mut self, addr: u16, value: u8, inc: i64) {
        self.clk_add(inc - 2);
        let da = (addr.wrapping_add(self.core.reg_x as u16) & 0xff) | (addr & 0xff00);
        self.load_dummy(da);
        self.clk_add(2);
        let mut tmp2 = addr.wrapping_add(self.core.reg_x as u16);
        if ((addr & 0xff) as u32 + self.core.reg_x as u32) > 0xff {
            tmp2 = (tmp2 & 0xff) | ((value as u16) << 8);
        }
        self.store(tmp2, value);
    }
    // PORT OF: vice/src/6510core.c:685-691 (STORE_ABS_Y). ts:697-702
    fn store_abs_y(&mut self, addr: u16, value: u8, inc: i64) {
        self.clk_add(inc - 2);
        let da = (addr.wrapping_add(self.core.reg_y as u16) & 0xff) | (addr & 0xff00);
        self.load_dummy(da);
        self.clk_add(2);
        self.store(addr.wrapping_add(self.core.reg_y as u16), value);
    }
    // PORT OF: vice/src/6510core.c:693-697 (STORE_ABS_Y_RMW). ts:704-707
    fn store_abs_y_rmw(&mut self, addr: u16, value: u8, inc: i64) {
        self.clk_add(inc);
        self.store(addr.wrapping_add(self.core.reg_y as u16), value);
    }
    // PORT OF: vice/src/6510core.c:699-711 (STORE_ABS_SH_Y). ts:709-718
    fn store_abs_sh_y(&mut self, addr: u16, value: u8, inc: i64) {
        self.clk_add(inc - 2);
        let da = (addr.wrapping_add(self.core.reg_y as u16) & 0xff) | (addr & 0xff00);
        self.load_dummy(da);
        self.clk_add(2);
        let mut tmp2 = addr.wrapping_add(self.core.reg_y as u16);
        if ((addr & 0xff) as u32 + self.core.reg_y as u32) > 0xff {
            tmp2 = (tmp2 & 0xff) | ((value as u16) << 8);
        }
        self.store(tmp2, value);
    }

    #[inline]
    fn inc_pc(&mut self, value: u16) {
        self.core.reg_pc = self.core.reg_pc.wrapping_add(value);
    }

    // RMW dummy stores (6510core.c:719-734). ts:725-733
    fn dummy_store_abs_rmw(&mut self, addr: u16, value: u8) {
        self.store_dummy(addr, value);
    }
    fn dummy_store_abs_x_rmw(&mut self, addr: u16, value: u8) {
        self.store_dummy(addr.wrapping_add(self.core.reg_x as u16), value);
    }
    fn dummy_store_abs_y_rmw(&mut self, addr: u16, value: u8) {
        self.store_dummy(addr.wrapping_add(self.core.reg_y as u16), value);
    }

    // -- function-table dispatch helpers (the load_func/store_func/dummy_func
    //    that the TS passes as arrow fns into RMW ops). ts:830-832 --
    #[inline]
    fn call_load(&mut self, f: LoadFn, a: u16) -> u8 {
        match f {
            LoadFn::Abs => self.load_abs(a),
            LoadFn::Zero => self.load_zero(a as u8),
            LoadFn::AbsXRmw => self.load_abs_x_rmw(a),
            LoadFn::AbsYRmw => self.load_abs_y_rmw(a),
        }
    }
    #[inline]
    fn call_store(&mut self, f: StoreFn, a: u16, v: u8, inc: i64) {
        match f {
            StoreFn::Abs => self.store_abs(a, v, inc),
            StoreFn::AbsX => self.store_abs_x(a, v, inc),
            StoreFn::AbsY => self.store_abs_y(a, v, inc),
            StoreFn::AbsXRmw => self.store_abs_x_rmw(a, v, inc),
            StoreFn::AbsYRmw => self.store_abs_y_rmw(a, v, inc),
        }
    }
    #[inline]
    fn call_dummy(&mut self, f: DummyFn, a: u16, v: u8) {
        match f {
            DummyFn::AbsRmw => self.dummy_store_abs_rmw(a, v),
            DummyFn::AbsXRmw => self.dummy_store_abs_x_rmw(a, v),
            DummyFn::AbsYRmw => self.dummy_store_abs_y_rmw(a, v),
        }
    }

    // =========================================================================
    // Opcode helpers — match VICE macro names (6510core.c:758-2012). ts:736-1653
    // =========================================================================

    // PORT OF: vice/src/6510core.c:758-791 (ADC). ts:740-766
    fn adc(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        let tmp_value = value as u32;
        self.clk_add(clk_inc);
        let tmp: u32;
        if self.local_decimal() {
            let mut t: i32 = (self.core.reg_a as i32 & 0xf)
                + (tmp_value as i32 & 0xf)
                + (self.core.reg_p as i32 & 0x1);
            if t > 0x9 {
                t += 0x6;
            }
            if t <= 0x0f {
                t = (t & 0xf) + (self.core.reg_a as i32 & 0xf0) + (tmp_value as i32 & 0xf0);
            } else {
                t = (t & 0xf) + (self.core.reg_a as i32 & 0xf0) + (tmp_value as i32 & 0xf0) + 0x10;
            }
            self.local_set_zero(
                ((self.core.reg_a as u32 + tmp_value + (self.core.reg_p as u32 & 0x1)) & 0xff) == 0,
            );
            self.local_set_sign(t & 0x80 != 0);
            self.local_set_overflow(
                ((self.core.reg_a as i32 ^ t) & 0x80) != 0
                    && ((self.core.reg_a as i32 ^ tmp_value as i32) & 0x80) == 0,
            );
            if (t & 0x1f0) > 0x90 {
                t += 0x60;
            }
            self.local_set_carry((t & 0xff0) > 0xf0);
            tmp = t as u32;
        } else {
            tmp = tmp_value + self.core.reg_a as u32 + (self.core.reg_p as u32 & P_CARRY as u32);
            self.local_set_nz((tmp & 0xff) as u8);
            self.local_set_overflow(
                ((self.core.reg_a as u32 ^ tmp_value) & 0x80) == 0
                    && ((self.core.reg_a as u32 ^ tmp) & 0x80) != 0,
            );
            self.local_set_carry(tmp > 0xff);
        }
        self.core.reg_a = (tmp & 0xff) as u8;
        self.inc_pc(pc_inc);
    }

    // PORT OF: vice/src/6510core.c:793-800 (ANC). ts:769-775
    fn anc(&mut self, value: u8, pc_inc: u16) {
        let tmp = self.core.reg_a & value;
        self.core.reg_a = tmp;
        self.local_set_nz(tmp);
        self.local_set_carry(self.local_sign());
        self.inc_pc(pc_inc);
    }

    // PORT OF: vice/src/6510core.c:802-809 (AND). ts:778-784
    fn and(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        let tmp = self.core.reg_a & value;
        self.core.reg_a = tmp;
        self.local_set_nz(tmp);
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
    }

    // PORT OF: vice/src/6510core.c:884-893 (ANE). ts:787-792
    fn ane(&mut self, value: u8, pc_inc: u16) {
        let tmp = (self.core.reg_a | ANE_MAGIC) & self.core.reg_x & value;
        self.core.reg_a = tmp;
        self.local_set_nz(tmp);
        self.inc_pc(pc_inc);
    }

    // PORT OF: vice/src/6510core.c:896-927 (ARR). ts:795-823
    fn arr(&mut self, value: u8, pc_inc: u16) {
        let tmp = (self.core.reg_a & value) as u32;
        if self.local_decimal() {
            let mut tmp_2 = tmp;
            tmp_2 |= (self.core.reg_p as u32 & P_CARRY as u32) << 8;
            tmp_2 >>= 1;
            self.local_set_sign(self.local_carry());
            self.local_set_zero(tmp_2 == 0);
            self.local_set_overflow((tmp_2 ^ tmp) & 0x40 != 0);
            if ((tmp & 0xf) + (tmp & 0x1)) > 0x5 {
                tmp_2 = (tmp_2 & 0xf0) | ((tmp_2 + 0x6) & 0xf);
            }
            if ((tmp & 0xf0) + (tmp & 0x10)) > 0x50 {
                tmp_2 = (tmp_2 & 0x0f) | ((tmp_2 + 0x60) & 0xf0);
                self.local_set_carry(true);
            } else {
                self.local_set_carry(false);
            }
            self.core.reg_a = (tmp_2 & 0xff) as u8;
        } else {
            let mut tmp = tmp;
            tmp |= (self.core.reg_p as u32 & P_CARRY as u32) << 8;
            tmp >>= 1;
            self.local_set_nz((tmp & 0xff) as u8);
            self.local_set_carry(tmp & 0x40 != 0);
            self.local_set_overflow(((tmp & 0x40) ^ ((tmp & 0x20) << 1)) != 0);
            self.core.reg_a = (tmp & 0xff) as u8;
        }
        self.inc_pc(pc_inc);
    }

    // PORT OF: vice/src/6510core.c:929-943 (ASL). ts:827-844
    fn asl(&mut self, addr: u16, pc_inc: u16, load_func: LoadFn, store_func: StoreFn, dummy_func: DummyFn) {
        let tmp_addr = addr;
        let mut tmp_value = self.call_load(load_func, tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.call_dummy(dummy_func, tmp_addr, tmp_value);
        self.local_set_carry(tmp_value & 0x80 != 0);
        tmp_value = (tmp_value as u16).wrapping_shl(1) as u8;
        self.local_set_nz(tmp_value);
        self.inc_pc(pc_inc);
        self.call_store(store_func, tmp_addr, tmp_value, 1);
    }
    // PORT OF: vice/src/6510core.c:945-953 (ASL_A). ts:846-853
    fn asl_a(&mut self) {
        let mut tmp = self.core.reg_a;
        self.local_set_carry(tmp & 0x80 != 0);
        tmp = (tmp as u16).wrapping_shl(1) as u8;
        self.core.reg_a = tmp;
        self.local_set_nz(tmp);
        self.inc_pc(1);
    }
    // PORT OF: vice/src/6510core.c:955-963 (ASR). ts:855-862
    fn asr(&mut self, value: u8, pc_inc: u16) {
        let mut tmp = self.core.reg_a & value;
        self.local_set_carry(tmp & 0x01 != 0);
        tmp >>= 1;
        self.core.reg_a = tmp;
        self.local_set_nz(tmp);
        self.inc_pc(pc_inc);
    }
    // PORT OF: vice/src/6510core.c:965-975 (BIT). ts:864-871
    fn bit(&mut self, value: u8, pc_inc: u16) {
        let tmp = value;
        self.clk_add(1);
        self.local_set_sign(tmp & 0x80 != 0);
        self.local_set_overflow(tmp & 0x40 != 0);
        self.local_set_zero((tmp & self.core.reg_a) == 0);
        self.inc_pc(pc_inc);
    }
    // PORT OF: vice/src/6510core.c:978-995 (BRANCH). ts:873-887
    fn branch(&mut self, cond: bool, value: u8) {
        self.inc_pc(2);
        if cond {
            let dest_addr = self.core.reg_pc.wrapping_add((value as i8) as u16);
            self.fetch_param_dummy(self.core.reg_pc);
            self.clk_add(CLK_BRANCH2 as i64);
            if (self.core.reg_pc ^ dest_addr) & 0xff00 != 0 {
                self.load_dummy((self.core.reg_pc & 0xff00) | (dest_addr & 0xff));
                self.clk_add(CLK_BRANCH2 as i64);
            } else {
                self.opcode_delays_interrupt();
            }
            self.jump(dest_addr);
        }
    }

    // PORT OF: vice/src/6510core.c:998-1038 (BRK). ts:890-919
    fn brk(&mut self) {
        self.inc_pc(2);
        self.local_set_break(true);
        self.push(((self.core.reg_pc >> 8) & 0xff) as u8);
        self.push((self.core.reg_pc & 0xff) as u8);
        self.clk_add(CLK_BRK as i64 - 3);
        let st = self.local_status();
        self.push(st);
        self.clk_add(1);
        self.process_alarms();
        let handler_vector: u16;
        if (self.int.global_pending_int & IK_NMI) != 0
            && (self.core.clk >= self.int.nmi_clk + INTERRUPT_DELAY)
        {
            self.local_set_interrupt(true);
            self.int.interrupt_ack_nmi();
            handler_vector = 0xfffa;
        } else if (self.int.global_pending_int & (IK_IRQ | IK_IRQPEND)) != 0
            && !self.local_interrupt()
            && (self.core.clk >= self.int.irq_clk + INTERRUPT_DELAY)
        {
            self.local_set_interrupt(true);
            self.int.interrupt_ack_irq();
            handler_vector = 0xfffe;
        } else {
            self.local_set_interrupt(true);
            handler_vector = 0xfffe;
        }
        let addr = self.load_addr(handler_vector);
        self.jump(addr);
        self.clk_add(2);
    }

    // Single-op flag helpers (6510core.c:1040-1065). ts:922-929
    fn clc(&mut self) {
        self.inc_pc(1);
        self.local_set_carry(false);
    }
    fn cld(&mut self) {
        self.inc_pc(1);
        self.local_set_decimal(false);
    }
    fn cli(&mut self) {
        self.inc_pc(1);
        if self.local_interrupt() {
            self.opcode_enables_irq();
        }
        self.local_set_interrupt(false);
    }
    fn clv(&mut self) {
        self.inc_pc(1);
        self.local_set_overflow(false);
    }

    // CMP / CPX / CPY (6510core.c:1067-1098). ts:939-959
    fn cmp(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        let tmp = (self.core.reg_a as u32).wrapping_sub(value as u32);
        self.local_set_carry(tmp < 0x100);
        self.local_set_nz((tmp & 0xff) as u8);
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
    }
    fn cpx(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        let tmp = (self.core.reg_x as u32).wrapping_sub(value as u32);
        self.local_set_carry(tmp < 0x100);
        self.local_set_nz((tmp & 0xff) as u8);
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
    }
    fn cpy(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        let tmp = (self.core.reg_y as u32).wrapping_sub(value as u32);
        self.local_set_carry(tmp < 0x100);
        self.local_set_nz((tmp & 0xff) as u8);
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
    }

    // DCP (6510core.c:1100-1115). ts:962-979
    fn dcp(&mut self, addr: u16, clk_inc1: i64, pc_inc: u16, load_func: LoadFn, store_func: StoreFn, dummy_func: DummyFn) {
        let tmp_addr = addr;
        self.clk_add(clk_inc1);
        let mut tmp = self.call_load(load_func, tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.call_dummy(dummy_func, tmp_addr, tmp);
        tmp = tmp.wrapping_sub(1);
        self.local_set_carry(self.core.reg_a >= tmp);
        self.local_set_nz(self.core.reg_a.wrapping_sub(tmp));
        self.inc_pc(pc_inc);
        self.call_store(store_func, tmp_addr, tmp, 1);
    }
    // PORT OF: vice/src/6510core.c:1117-1135 (DCP_IND_Y). ts:981-996
    fn dcp_ind_y(&mut self, addr: u8) {
        let mut tmp_addr = self.load_zero_addr(addr);
        self.clk_add(2);
        let da = (tmp_addr & 0xff00) | (tmp_addr.wrapping_add(self.core.reg_y as u16) & 0xff);
        self.load_dummy(da);
        self.clk_add_dummy(1);
        tmp_addr = tmp_addr.wrapping_add(self.core.reg_y as u16);
        let mut tmp = self.load(tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.dummy_store_abs_rmw(tmp_addr, tmp);
        tmp = tmp.wrapping_sub(1);
        self.local_set_carry(self.core.reg_a >= tmp);
        self.local_set_nz(self.core.reg_a.wrapping_sub(tmp));
        self.inc_pc(2);
        self.store_abs(tmp_addr, tmp, 1);
    }

    // PORT OF: vice/src/6510core.c:1137-1150 (DEC). ts:999-1014
    fn dec(&mut self, addr: u16, pc_inc: u16, load_func: LoadFn, store_func: StoreFn, dummy_func: DummyFn) {
        let tmp_addr = addr;
        let mut tmp = self.call_load(load_func, tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.call_dummy(dummy_func, tmp_addr, tmp);
        tmp = tmp.wrapping_sub(1);
        self.local_set_nz(tmp);
        self.inc_pc(pc_inc);
        self.call_store(store_func, tmp_addr, tmp, 1);
    }
    fn dex(&mut self) {
        self.core.reg_x = self.core.reg_x.wrapping_sub(1);
        self.local_set_nz(self.core.reg_x);
        self.inc_pc(1);
    }
    fn dey(&mut self) {
        self.core.reg_y = self.core.reg_y.wrapping_sub(1);
        self.local_set_nz(self.core.reg_y);
        self.inc_pc(1);
    }

    fn eor(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        let tmp = self.core.reg_a ^ value;
        self.core.reg_a = tmp;
        self.local_set_nz(tmp);
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
    }

    // PORT OF: vice/src/6510core.c:1175-1188 (INC). ts:1027-1042
    fn inc(&mut self, addr: u16, pc_inc: u16, load_func: LoadFn, store_func: StoreFn, dummy_func: DummyFn) {
        let tmp_addr = addr;
        let mut tmp = self.call_load(load_func, tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.call_dummy(dummy_func, tmp_addr, tmp);
        tmp = tmp.wrapping_add(1);
        self.local_set_nz(tmp);
        self.inc_pc(pc_inc);
        self.call_store(store_func, tmp_addr, tmp, 1);
    }
    fn inx(&mut self) {
        self.core.reg_x = self.core.reg_x.wrapping_add(1);
        self.local_set_nz(self.core.reg_x);
        self.inc_pc(1);
    }
    fn iny(&mut self) {
        self.core.reg_y = self.core.reg_y.wrapping_add(1);
        self.local_set_nz(self.core.reg_y);
        self.inc_pc(1);
    }

    // ISB (6510core.c:1204-1218). ts:1047-1063
    fn isb(&mut self, addr: u16, clk_inc1: i64, pc_inc: u16, load_func: LoadFn, store_func: StoreFn, dummy_func: DummyFn) {
        let my_addr = addr;
        self.clk_add(clk_inc1);
        let mut my_src = self.call_load(load_func, my_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.call_dummy(dummy_func, my_addr, my_src);
        my_src = my_src.wrapping_add(1);
        self.sbc(my_src, 0, 0);
        self.inc_pc(pc_inc);
        self.call_store(store_func, my_addr, my_src, 1);
    }
    // PORT OF: vice/src/6510core.c:1220-1237 (ISB_IND_Y). ts:1065-1079
    fn isb_ind_y(&mut self, addr: u8) {
        let mut my_addr = self.load_zero_addr(addr);
        self.clk_add(2);
        let da = (my_addr & 0xff00) | (my_addr.wrapping_add(self.core.reg_y as u16) & 0xff);
        self.load_dummy(da);
        self.clk_add_dummy(1);
        my_addr = my_addr.wrapping_add(self.core.reg_y as u16);
        let mut my_src = self.load(my_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.dummy_store_abs_rmw(my_addr, my_src);
        my_src = my_src.wrapping_add(1);
        self.sbc(my_src, 0, 0);
        self.inc_pc(2);
        self.store_abs(my_addr, my_src, 1);
    }

    // PORT OF: vice/src/6510core.c:1242-1260 (JAM_02). ts:1086-1112
    // Returns true if a trap was applied (skip dispatch); false if jammed.
    fn jam_02(&mut self) -> bool {
        // STATIC_ASSERT(TRAP_OPCODE == 0x02). ts:1087-1089
        debug_assert_eq!(TRAP_OPCODE, 0x02);
        // ROM_TRAP_ALLOWED() is always 1 in DRIVE_CPU build.
        let trap_result = self.bus.trap_handler();
        if trap_result == 0xffff_ffff {
            // Real JAM.
            self.core.is_jammed = true;
            self.rewind_fetch_opcode();
            self.jam_result = self.host_jam();
            return false;
        }
        if trap_result != 0 {
            // Trap-replaced opcode: rewind + replay with the new opcode.
            self.rewind_fetch_opcode();
            self.core.last_opcode_info = trap_result & 0xff;
            return true;
        }
        // trap_result == 0: trap handled in-place, just continue.
        true
    }

    // PORT OF: vice/src/drive/drivecpu.c:411 — JAM() = drivecpu_jam(drv). ts:1117-1123
    fn host_jam(&mut self) -> i32 {
        match self.bus.jam() {
            None => {
                self.clk_add(1); // default path: drivecpu.c:537 (`default: CLK++`).
                JAM_NONE
            }
            Some(code) => code,
        }
    }

    fn jmp(&mut self, addr: u16) {
        self.jump(addr);
    }

    // PORT OF: vice/src/6510core.c:1267-1275 (JMP_IND). ts:1128-1134
    fn jmp_ind(&mut self, p2: u16) {
        let mut dest_addr = self.load(p2) as u16;
        self.clk_add(1);
        dest_addr |= (self.load((p2 & 0xff00) | (p2.wrapping_add(1) & 0xff)) as u16) << 8;
        self.clk_add(1);
        self.jump(dest_addr);
    }

    // PORT OF: vice/src/6510core.c:1284-1301 (JSR). ts:1137-1148
    fn jsr(&mut self, p1: u8) {
        self.load_dummy(0x100 + self.core.reg_sp as u16);
        self.clk_add(1);
        self.inc_pc(2);
        self.clk_add(2);
        self.push(((self.core.reg_pc >> 8) & 0xff) as u8);
        self.push((self.core.reg_pc & 0xff) as u8);
        let addr_msb = self.load(self.core.reg_pc);
        let tmp_addr = (p1 as u16) | ((addr_msb as u16) << 8);
        self.clk_add(CLK_JSR_INT_CYCLE as i64);
        self.jump(tmp_addr);
    }

    // PORT OF: vice/src/6510core.c:1303-1311 (LAS). ts:1151-1158
    fn las(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        self.core.reg_sp &= value;
        self.core.reg_x = self.core.reg_sp;
        self.core.reg_a = self.core.reg_sp;
        self.local_set_nz(self.core.reg_sp);
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
    }
    // PORT OF: vice/src/6510core.c:1313-1321 (LAX). ts:1160-1167
    fn lax(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        let tmp = value;
        self.core.reg_x = tmp;
        self.core.reg_a = tmp;
        self.local_set_nz(tmp);
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
    }
    // PORT OF: vice/src/6510core.c:1323-1330 (LDA). ts:1169-1175
    fn lda(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        let tmp = value;
        self.core.reg_a = tmp;
        self.clk_add(clk_inc);
        self.local_set_nz(tmp);
        self.inc_pc(pc_inc);
    }
    fn ldx(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        self.core.reg_x = value;
        self.local_set_nz(self.core.reg_x);
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
    }
    fn ldy(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        self.core.reg_y = value;
        self.local_set_nz(self.core.reg_y);
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
    }

    // PORT OF: vice/src/6510core.c:1348-1362 (LSR). ts:1190-1206
    fn lsr(&mut self, addr: u16, pc_inc: u16, load_func: LoadFn, store_func: StoreFn, dummy_func: DummyFn) {
        let tmp_addr = addr;
        let mut tmp = self.call_load(load_func, tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.call_dummy(dummy_func, tmp_addr, tmp);
        self.local_set_carry(tmp & 0x01 != 0);
        tmp >>= 1;
        self.local_set_nz(tmp);
        self.inc_pc(pc_inc);
        self.call_store(store_func, tmp_addr, tmp, 1);
    }
    fn lsr_a(&mut self) {
        let mut tmp = self.core.reg_a;
        self.local_set_carry(tmp & 0x01 != 0);
        tmp >>= 1;
        self.core.reg_a = tmp;
        self.local_set_nz(tmp);
        self.inc_pc(1);
    }
    // PORT OF: vice/src/6510core.c:1427-1435 (LXA). ts:1216-1222
    fn lxa(&mut self, value: u8, pc_inc: u16) {
        let tmp = (self.core.reg_a | LXA_MAGIC) & value;
        self.core.reg_x = tmp;
        self.core.reg_a = tmp;
        self.local_set_nz(tmp);
        self.inc_pc(pc_inc);
    }

    fn ora(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        let tmp = self.core.reg_a | value;
        self.core.reg_a = tmp;
        self.local_set_nz(tmp);
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
    }

    // NOOP family (6510core.c:1447-1465). ts:1233-1247
    fn noop(&mut self, clk_inc: i64, pc_inc: u16) {
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
    }
    fn noop_imm(&mut self, pc_inc: u16) {
        self.inc_pc(pc_inc);
    }
    fn noop_abs(&mut self, p2: u16) {
        self.load(p2);
        self.clk_add(1);
        self.inc_pc(3);
    }
    fn noop_abs_x(&mut self, p2: u16) {
        self.noop_load_abs_x(p2);
        self.clk_add(1);
        self.inc_pc(3);
    }

    // PHA / PHP / PLA / PLP (6510core.c:1467-1507). ts:1250-1271
    fn pha(&mut self) {
        self.clk_add(CLK_STACK_PUSH as i64);
        self.push(self.core.reg_a);
        self.inc_pc(1);
    }
    fn php(&mut self) {
        self.clk_add(CLK_STACK_PUSH as i64);
        let st = self.local_status() | P_BREAK;
        self.push(st);
        self.inc_pc(1);
    }
    fn pla(&mut self) {
        self.clk_add(CLK_STACK_PULL as i64);
        self.load_dummy(0x100 + self.core.reg_sp as u16);
        let tmp = self.pull();
        self.core.reg_a = tmp;
        self.local_set_nz(tmp);
        self.inc_pc(1);
    }
    fn plp(&mut self) {
        self.load_dummy(0x100 + self.core.reg_sp as u16);
        let s = self.pull();
        if (s & P_INTERRUPT) == 0 && self.local_interrupt() {
            self.opcode_enables_irq();
        } else if (s & P_INTERRUPT) != 0 && !self.local_interrupt() {
            self.opcode_disables_irq();
        }
        self.clk_add(CLK_STACK_PULL as i64);
        self.local_set_status(s);
        self.inc_pc(1);
    }

    // RLA (6510core.c:1509-1526). ts:1274-1293
    fn rla(&mut self, addr: u16, clk_inc1: i64, pc_inc: u16, load_func: LoadFn, store_func: StoreFn, dummy_func: DummyFn) {
        let tmp_addr = addr;
        self.clk_add(clk_inc1);
        let tmp_loaded = self.call_load(load_func, tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.call_dummy(dummy_func, tmp_addr, tmp_loaded);
        let tmp =
            ((tmp_loaded as u16) << 1) | (self.core.reg_p as u16 & P_CARRY as u16);
        self.local_set_carry(tmp & 0x100 != 0);
        let tmp2 = self.core.reg_a & (tmp as u8);
        self.core.reg_a = tmp2;
        self.local_set_nz(tmp2);
        self.inc_pc(pc_inc);
        self.call_store(store_func, tmp_addr, (tmp & 0xff) as u8, 1);
    }
    // PORT OF: vice/src/6510core.c:1528-1548 (RLA_IND_Y). ts:1295-1312
    fn rla_ind_y(&mut self, addr: u8) {
        let mut tmp_addr = self.load_zero_addr(addr);
        self.clk_add(2);
        let da = (tmp_addr & 0xff00) | (tmp_addr.wrapping_add(self.core.reg_y as u16) & 0xff);
        self.load_dummy(da);
        self.clk_add_dummy(1);
        tmp_addr = tmp_addr.wrapping_add(self.core.reg_y as u16);
        let tmp_loaded = self.load(tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.dummy_store_abs_rmw(tmp_addr, tmp_loaded);
        let tmp = ((tmp_loaded as u16) << 1) | (self.core.reg_p as u16 & P_CARRY as u16);
        self.local_set_carry(tmp & 0x100 != 0);
        let tmp2 = self.core.reg_a & (tmp as u8);
        self.core.reg_a = tmp2;
        self.local_set_nz(tmp2);
        self.inc_pc(2);
        self.store_abs(tmp_addr, (tmp & 0xff) as u8, 1);
    }

    // ROL (6510core.c:1550-1564). ts:1315-1331
    fn rol(&mut self, addr: u16, pc_inc: u16, load_func: LoadFn, store_func: StoreFn, dummy_func: DummyFn) {
        let tmp_addr = addr;
        let tmp_loaded = self.call_load(load_func, tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.call_dummy(dummy_func, tmp_addr, tmp_loaded);
        let tmp = ((tmp_loaded as u16) << 1) | (self.core.reg_p as u16 & P_CARRY as u16);
        self.local_set_carry(tmp & 0x100 != 0);
        self.local_set_nz((tmp & 0xff) as u8);
        self.inc_pc(pc_inc);
        self.call_store(store_func, tmp_addr, (tmp & 0xff) as u8, 1);
    }
    fn rol_a(&mut self) {
        let mut tmp = (self.core.reg_a as u16) << 1;
        tmp |= self.core.reg_p as u16 & P_CARRY as u16;
        self.core.reg_a = (tmp & 0xff) as u8;
        self.local_set_nz((tmp & 0xff) as u8);
        self.local_set_carry(tmp & 0x100 != 0);
        self.inc_pc(1);
    }
    // ROR (6510core.c:1577-1594). ts:1341-1358
    fn ror(&mut self, addr: u16, pc_inc: u16, load_func: LoadFn, store_func: StoreFn, dummy_func: DummyFn) {
        let tmp_addr = addr;
        let mut src = self.call_load(load_func, tmp_addr) as u16;
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.call_dummy(dummy_func, tmp_addr, src as u8);
        if self.core.reg_p & P_CARRY != 0 {
            src |= 0x100;
        }
        self.local_set_carry(src & 0x01 != 0);
        src >>= 1;
        self.local_set_nz((src & 0xff) as u8);
        self.inc_pc(pc_inc);
        self.call_store(store_func, tmp_addr, (src & 0xff) as u8, 1);
    }
    fn ror_a(&mut self) {
        let tmp = self.core.reg_a;
        let tmp2 = ((tmp >> 1) | ((self.core.reg_p as u16) << 7) as u8) & 0xff;
        self.local_set_carry(tmp & 0x01 != 0);
        self.core.reg_a = tmp2;
        self.local_set_nz(tmp2);
        self.inc_pc(1);
    }

    // RRA (6510core.c:1606-1625). ts:1369-1387
    fn rra(&mut self, addr: u16, clk_inc1: i64, pc_inc: u16, load_func: LoadFn, store_func: StoreFn, dummy_func: DummyFn) {
        let tmp_addr = addr;
        self.clk_add(clk_inc1);
        let src = self.call_load(load_func, tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.call_dummy(dummy_func, tmp_addr, src);
        let mut my_temp = src >> 1;
        if self.core.reg_p & P_CARRY != 0 {
            my_temp |= 0x80;
        }
        self.local_set_carry(src & 0x1 != 0);
        self.inc_pc(pc_inc);
        self.adc(my_temp, 0, 0);
        self.call_store(store_func, tmp_addr, my_temp, 1);
    }
    // PORT OF: vice/src/6510core.c:1627-1650 (RRA_IND_Y). ts:1389-1405
    fn rra_ind_y(&mut self, addr: u8) {
        let mut my_tmp_addr = self.load_zero_addr(addr);
        self.clk_add(2);
        let da = (my_tmp_addr & 0xff00) | (my_tmp_addr.wrapping_add(self.core.reg_y as u16) & 0xff);
        self.load_dummy(da);
        self.clk_add_dummy(1);
        my_tmp_addr = my_tmp_addr.wrapping_add(self.core.reg_y as u16);
        let src = self.load(my_tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.dummy_store_abs_rmw(my_tmp_addr, src);
        self.inc_pc(2);
        let mut my_temp = src >> 1;
        if self.core.reg_p & P_CARRY != 0 {
            my_temp |= 0x80;
        }
        self.local_set_carry(src & 0x1 != 0);
        self.adc(my_temp, 0, 0);
        self.store_abs(my_tmp_addr, my_temp, 1);
    }

    // RTI / RTS (6510core.c:1657-1684). ts:1408-1426
    fn rti(&mut self) {
        self.clk_add(CLK_RTI as i64);
        self.load_dummy(0x100 + self.core.reg_sp as u16);
        let mut tmp = self.pull() as u16;
        self.local_set_status((tmp & 0xff) as u8);
        tmp = self.pull() as u16;
        tmp |= (self.pull() as u16) << 8;
        self.jump(tmp);
    }
    fn rts(&mut self) {
        self.clk_add(CLK_RTS as i64);
        self.load_dummy(0x100 + self.core.reg_sp as u16);
        let mut tmp = self.pull() as u16;
        tmp |= (self.pull() as u16) << 8;
        self.jump(tmp);
        self.fetch_param(self.core.reg_pc);
        self.clk_add(CLK_INT_CYCLE as i64);
        self.inc_pc(1);
    }

    // SAX family (6510core.c:1686-1702). ts:1429-1440
    fn sax(&mut self, addr: u16, clk_inc1: i64, clk_inc2: i64, pc_inc: u16) {
        self.clk_add(clk_inc1);
        let tmp = addr;
        self.clk_add(clk_inc2);
        self.inc_pc(pc_inc);
        self.store(tmp, self.core.reg_a & self.core.reg_x);
    }
    fn sax_zero(&mut self, addr: u8, clk_inc: i64, pc_inc: u16) {
        self.clk_add(clk_inc);
        self.store_zero(addr, self.core.reg_a & self.core.reg_x);
        self.inc_pc(pc_inc);
    }

    // SBC (6510core.c:1704-1733). ts:1443-1466
    fn sbc(&mut self, value: u8, clk_inc: i64, pc_inc: u16) {
        let src = value as u32;
        self.clk_add(clk_inc);
        let tmp = (self.core.reg_a as u32)
            .wrapping_sub(src)
            .wrapping_sub(if self.core.reg_p & P_CARRY != 0 { 0 } else { 1 })
            & 0xffff;
        if self.core.reg_p & P_DECIMAL != 0 {
            let mut tmp_a = (self.core.reg_a as i32 & 0xf)
                - (src as i32 & 0xf)
                - (if self.core.reg_p & P_CARRY != 0 { 0 } else { 1 });
            if tmp_a & 0x10 != 0 {
                tmp_a = ((tmp_a - 6) & 0xf)
                    | (((self.core.reg_a as i32 & 0xf0) - (src as i32 & 0xf0) - 0x10) & 0xffff);
            } else {
                tmp_a = (tmp_a & 0xf)
                    | (((self.core.reg_a as i32 & 0xf0) - (src as i32 & 0xf0)) & 0xffff);
            }
            if tmp_a & 0x100 != 0 {
                tmp_a -= 0x60;
            }
            self.local_set_carry(tmp < 0x100);
            self.local_set_nz((tmp & 0xff) as u8);
            self.local_set_overflow(
                ((self.core.reg_a as u32 ^ tmp) & 0x80) != 0
                    && ((self.core.reg_a as u32 ^ src) & 0x80) != 0,
            );
            self.core.reg_a = (tmp_a & 0xff) as u8;
        } else {
            self.local_set_nz((tmp & 0xff) as u8);
            self.local_set_carry(tmp < 0x100);
            self.local_set_overflow(
                ((self.core.reg_a as u32 ^ tmp) & 0x80) != 0
                    && ((self.core.reg_a as u32 ^ src) & 0x80) != 0,
            );
            self.core.reg_a = (tmp & 0xff) as u8;
        }
        self.inc_pc(pc_inc);
    }
    // PORT OF: vice/src/6510core.c:1735-1745 (SBX). ts:1468-1475
    fn sbx(&mut self, value: u8, pc_inc: u16) {
        let tmp = value as u32;
        self.inc_pc(pc_inc);
        let tmp = ((self.core.reg_a & self.core.reg_x) as u32).wrapping_sub(tmp) & 0xffff;
        self.local_set_carry(tmp < 0x100);
        self.core.reg_x = (tmp & 0xff) as u8;
        self.local_set_nz(self.core.reg_x);
    }

    // Single-flag setters (6510core.c:1748-1767). ts:1478-1484
    fn sec(&mut self) {
        self.local_set_carry(true);
        self.inc_pc(1);
    }
    fn sed(&mut self) {
        self.local_set_decimal(true);
        self.inc_pc(1);
    }
    fn sei(&mut self) {
        if !self.local_interrupt() {
            self.opcode_disables_irq();
        }
        self.local_set_interrupt(true);
        self.inc_pc(1);
    }

    // SHA / SHX / SHY / SHS (6510core.c:1769-1822). ts:1487-1517
    fn sha_abs_y(&mut self, addr: u16) {
        self.inc_pc(3);
        let v = self.core.reg_a & self.core.reg_x & (((addr >> 8) as u8).wrapping_add(1));
        self.store_abs_sh_y(addr, v, CLK_ABS_I_STORE2 as i64);
    }
    fn sha_ind_y(&mut self, addr: u8) {
        let mut tmp = self.load_zero_addr(addr);
        self.clk_add(2);
        self.load((tmp & 0xff00) | (tmp.wrapping_add(self.core.reg_y as u16) & 0xff));
        self.clk_add(CLK_IND_Y_W as i64);
        let val = self.core.reg_a & self.core.reg_x & (((tmp >> 8) as u8).wrapping_add(1));
        if ((tmp & 0xff) as u32 + self.core.reg_y as u32) > 0xff {
            tmp = (tmp.wrapping_add(self.core.reg_y as u16) & 0xff) | ((val as u16) << 8);
        } else {
            tmp = tmp.wrapping_add(self.core.reg_y as u16);
        }
        self.inc_pc(2);
        self.store(tmp, val);
    }
    fn shx_abs_y(&mut self, addr: u16) {
        self.inc_pc(3);
        let v = self.core.reg_x & (((addr >> 8) as u8).wrapping_add(1));
        self.store_abs_sh_y(addr, v, CLK_ABS_I_STORE2 as i64);
    }
    fn shy_abs_x(&mut self, addr: u16) {
        self.inc_pc(3);
        let v = self.core.reg_y & (((addr >> 8) as u8).wrapping_add(1));
        self.store_abs_sh_x(addr, v, CLK_ABS_I_STORE2 as i64);
    }
    fn shs_abs_y(&mut self, addr: u16) {
        self.inc_pc(3);
        let v = self.core.reg_a & self.core.reg_x & (((addr >> 8) as u8).wrapping_add(1));
        self.store_abs_sh_y(addr, v, CLK_ABS_I_STORE2 as i64);
        self.core.reg_sp = self.core.reg_a & self.core.reg_x;
    }

    // SLO (6510core.c:1824-1842). ts:1520-1539
    fn slo(&mut self, addr: u16, clk_inc1: i64, pc_inc: u16, load_func: LoadFn, store_func: StoreFn, dummy_func: DummyFn) {
        let tmp_addr = addr;
        self.clk_add(clk_inc1);
        let mut tmp = self.call_load(load_func, tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.call_dummy(dummy_func, tmp_addr, tmp);
        self.local_set_carry(tmp & 0x80 != 0);
        tmp = (tmp as u16).wrapping_shl(1) as u8;
        let tmp2 = self.core.reg_a | tmp;
        self.core.reg_a = tmp2;
        self.local_set_nz(tmp2);
        self.inc_pc(pc_inc);
        self.call_store(store_func, tmp_addr, tmp, 1);
    }
    // PORT OF: vice/src/6510core.c:1844-1865 (SLO_IND_Y). ts:1541-1558
    fn slo_ind_y(&mut self, addr: u8) {
        let mut tmp_addr = self.load_zero_addr(addr);
        self.clk_add(2);
        let da = (tmp_addr & 0xff00) | (tmp_addr.wrapping_add(self.core.reg_y as u16) & 0xff);
        self.load_dummy(da);
        self.clk_add_dummy(1);
        tmp_addr = tmp_addr.wrapping_add(self.core.reg_y as u16);
        let mut tmp = self.load(tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.dummy_store_abs_rmw(tmp_addr, tmp);
        self.local_set_carry(tmp & 0x80 != 0);
        tmp = (tmp as u16).wrapping_shl(1) as u8;
        let tmp2 = self.core.reg_a | tmp;
        self.core.reg_a = tmp2;
        self.local_set_nz(tmp2);
        self.inc_pc(2);
        self.store_abs(tmp_addr, tmp, 1);
    }

    // SRE (6510core.c:1867-1885). ts:1561-1580
    fn sre(&mut self, addr: u16, clk_inc1: i64, pc_inc: u16, load_func: LoadFn, store_func: StoreFn, dummy_func: DummyFn) {
        let tmp_addr = addr;
        self.clk_add(clk_inc1);
        let mut tmp = self.call_load(load_func, tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.call_dummy(dummy_func, tmp_addr, tmp);
        self.local_set_carry(tmp & 0x01 != 0);
        tmp >>= 1;
        let tmp2 = self.core.reg_a ^ tmp;
        self.core.reg_a = tmp2;
        self.local_set_nz(tmp2);
        self.inc_pc(pc_inc);
        self.call_store(store_func, tmp_addr, tmp, 1);
    }
    // PORT OF: vice/src/6510core.c:1887-1907 (SRE_IND_Y). ts:1582-1599
    fn sre_ind_y(&mut self, addr: u8) {
        let mut tmp_addr = self.load_zero_addr(addr);
        self.clk_add(2);
        let da = (tmp_addr & 0xff00) | (tmp_addr.wrapping_add(self.core.reg_y as u16) & 0xff);
        self.load_dummy(da);
        self.clk_add_dummy(1);
        tmp_addr = tmp_addr.wrapping_add(self.core.reg_y as u16);
        let mut tmp = self.load(tmp_addr);
        self.clk_add(1);
        self.clk_add_dummy(1);
        self.dummy_store_abs_rmw(tmp_addr, tmp);
        self.local_set_carry(tmp & 0x01 != 0);
        tmp >>= 1;
        let tmp2 = self.core.reg_a ^ tmp;
        self.core.reg_a = tmp2;
        self.local_set_nz(tmp2);
        self.inc_pc(2);
        self.store_abs(tmp_addr, tmp, 1);
    }

    // STA / STX / STY (6510core.c:1909-1970). ts:1602-1645
    fn sta(&mut self, addr: u16, clk_inc1: i64, clk_inc2: i64, pc_inc: u16, store_func: StoreFn) {
        self.clk_add(clk_inc1);
        let tmp = addr;
        self.inc_pc(pc_inc);
        self.call_store(store_func, tmp, self.core.reg_a, clk_inc2);
    }
    fn sta_zero(&mut self, addr: u8, clk_inc: i64, pc_inc: u16) {
        self.clk_add(clk_inc);
        self.store_zero(addr, self.core.reg_a);
        self.inc_pc(pc_inc);
    }
    fn sta_ind_y(&mut self, addr: u8) {
        let tmp = self.load_zero_addr(addr);
        self.clk_add(2);
        let da = (tmp & 0xff00) | (tmp.wrapping_add(self.core.reg_y as u16) & 0xff);
        self.load_dummy(da);
        self.clk_add(CLK_IND_Y_W as i64);
        self.inc_pc(2);
        self.store_ind(tmp.wrapping_add(self.core.reg_y as u16), self.core.reg_a);
    }
    fn stx(&mut self, addr: u16, clk_inc: i64, pc_inc: u16) {
        let tmp = addr;
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
        self.store(tmp, self.core.reg_x);
    }
    fn stx_zero(&mut self, addr: u8, clk_inc: i64, pc_inc: u16) {
        self.clk_add(clk_inc);
        self.store_zero(addr, self.core.reg_x);
        self.inc_pc(pc_inc);
    }
    fn sty(&mut self, addr: u16, clk_inc: i64, pc_inc: u16) {
        let tmp = addr;
        self.clk_add(clk_inc);
        self.inc_pc(pc_inc);
        self.store(tmp, self.core.reg_y);
    }
    fn sty_zero(&mut self, addr: u8, clk_inc: i64, pc_inc: u16) {
        self.clk_add(clk_inc);
        self.store_zero(addr, self.core.reg_y);
        self.inc_pc(pc_inc);
    }

    // Register transfers (6510core.c:1972-2011). ts:1648-1653
    fn tax(&mut self) {
        self.core.reg_x = self.core.reg_a;
        self.local_set_nz(self.core.reg_x);
        self.inc_pc(1);
    }
    fn tay(&mut self) {
        self.core.reg_y = self.core.reg_a;
        self.local_set_nz(self.core.reg_y);
        self.inc_pc(1);
    }
    fn tsx(&mut self) {
        self.core.reg_x = self.core.reg_sp;
        self.local_set_nz(self.core.reg_sp);
        self.inc_pc(1);
    }
    fn txa(&mut self) {
        self.core.reg_a = self.core.reg_x;
        self.local_set_nz(self.core.reg_x);
        self.inc_pc(1);
    }
    fn txs(&mut self) {
        self.core.reg_sp = self.core.reg_x;
        self.inc_pc(1);
    }
    fn tya(&mut self) {
        self.core.reg_a = self.core.reg_y;
        self.local_set_nz(self.core.reg_y);
        self.inc_pc(1);
    }

    // =========================================================================
    // SECTION H — DO_INTERRUPT (6510core.c:436-530). ts:2057-2120
    // Drive-CPU subset: NO monitor branches, NO DMA path.
    // =========================================================================
    #[allow(unused_assignments)] // ts:2059 keeps handler_vector=0xfffe init for fidelity
    fn do_interrupt(&mut self, int_kind: u32) {
        let mut ik = int_kind;
        let mut handler_vector: u16 = 0xfffe; // ts:2059

        if ik & (IK_IRQ | IK_IRQPEND | IK_NMI) != 0 {
            let clk = self.core.clk;
            let nmi_now = (ik & IK_NMI) != 0 && interrupt_check_nmi_delay(self.int, clk);
            // Evaluate the I-flag / DISABLES_IRQ gate BEFORE the delay check so the
            // `&mut self.int` borrow inside interrupt_check_irq_delay does not clash
            // with the immutable `self.core` reads (disjoint fields, sequenced here).
            let irq_gate = (ik & (IK_IRQ | IK_IRQPEND)) != 0
                && (!self.local_interrupt()
                    || opinfo_disables_irq(self.core.last_opcode_info) != 0);
            let irq_now = irq_gate && interrupt_check_irq_delay(self.int, clk);
            if nmi_now || irq_now {
                if NMI_CYCLES == 7 {
                    self.fetch_param_dummy(self.core.reg_pc);
                    self.clk_add(1);
                    self.fetch_param_dummy(self.core.reg_pc);
                    self.clk_add(1);
                }
                self.local_set_break(false);
                self.push(((self.core.reg_pc >> 8) & 0xff) as u8);
                self.push((self.core.reg_pc & 0xff) as u8);
                self.clk_add(2);
                let st = self.local_status();
                self.push(st);
                self.clk_add(1);
                self.local_set_interrupt(true);
                self.process_alarms();
                if (self.int.global_pending_int & IK_NMI) != 0
                    && (self.core.clk >= self.int.nmi_clk + INTERRUPT_DELAY)
                {
                    self.int.interrupt_ack_nmi();
                    handler_vector = 0xfffa;
                } else {
                    self.int.interrupt_ack_irq();
                    handler_vector = 0xfffe;
                }
                let addr = self.load_addr(handler_vector);
                self.jump(addr);
                self.set_last_opcode(0);
                self.clk_add(2);
            }
        }

        if ik & (IK_TRAP | IK_RESET) != 0 {
            if ik & IK_TRAP != 0 {
                // trap path goes through JAM_02; allow IK_RESET to chain.
                if self.int.global_pending_int & IK_RESET != 0 {
                    ik |= IK_RESET;
                }
            }
            if ik & IK_RESET != 0 {
                self.bus.cpu_reset();
                self.int.interrupt_ack_reset();
                self.core.d_bank_start = 0;
                self.core.d_bank_limit = 0;
                self.local_set_interrupt(true);
                self.core.is_jammed = false;
                let addr = self.load_addr(0xfffc);
                self.jump(addr);
            }
        }
        if ik & IK_MONITOR != 0 {
            // Monitor not ported (PL-5 forbids a stub).
        }
    }
}

// =============================================================================
// SECTION G (entry) — drive_6510core_execute.
//
// ONE call advances the drive CPU by exactly one opcode (or one interrupt
// dispatch), updating core.clk mid-opcode for each phase per VICE. PORT OF:
// vice/src/6510core.c:2281-3476 (DRIVE_CPU path) + drivecpu.c:131-440 macros.
// ts:331-2048.
//
// Return value: JAM_NONE on a normal opcode (or IRQ/NMI/RESET dispatch), or one
// of JAM_RESET_CPU / JAM_POWER_CYCLE / JAM_MONITOR on a JAM whose host handler
// returned one of those codes.
// =============================================================================
pub fn drive_6510core_execute<B: DriveCore6510Bus>(
    core: &mut DriveCore6510,
    bus: &mut B,
    int: &mut IntStatus,
) -> i32 {
    let mut ex = Exec {
        core,
        bus,
        int,
        jam_result: JAM_NONE,
    };

    // 1) Refresh + alarm prologue (6510core.c:2299-2308). ts:1660
    ex.process_alarms();

    // 2) HACK: jammed CPU clears IRQ/NMI flags + only RESET wakes (2310-2319). ts:1663-1670
    if ex.core.is_jammed {
        ex.int.interrupt_ack_irq();
        ex.int.global_pending_int &= !(IK_IRQ | IK_NMI);
        if ex.int.global_pending_int & IK_RESET != 0 {
            ex.core.is_jammed = false;
        }
    }

    // 3) Pending-interrupt dispatch (6510core.c:2321-2345). ts:1672-1688
    {
        if (ex.int.global_pending_int & IK_IRQ) == 0
            && (ex.int.global_pending_int & IK_IRQPEND) != 0
            && ex.int.irq_pending_clk <= ex.core.clk
        {
            ex.int.interrupt_ack_irq();
        }
        let pending_interrupt = ex.int.global_pending_int;
        if pending_interrupt != IK_NONE {
            ex.do_interrupt(pending_interrupt);
            if (ex.int.global_pending_int & IK_IRQ) == 0
                && (ex.int.global_pending_int & IK_IRQPEND) != 0
            {
                ex.int.global_pending_int &= !IK_IRQPEND;
            }
            ex.process_alarms();
        }
    }

    // 4) Opcode fetch (DRIVE_CPU non-8502 / non-DTV path). ts:1690-1714
    let reg_pc0 = ex.core.reg_pc;
    let p0: u8;
    let p1: u8;
    let p2: u16;
    if reg_pc0 < ex.core.d_bank_limit && ex.core.d_bank_base.is_some() {
        // Bank fast-path. ts:1694-1701
        let base = ex.core.d_bank_base.as_ref().unwrap();
        let ins = base[reg_pc0 as usize];
        let op1 = base[reg_pc0.wrapping_add(1) as usize];
        let op2 = base[reg_pc0.wrapping_add(2) as usize];
        p0 = ins;
        p1 = op1;
        p2 = (op1 as u16) | ((op2 as u16) << 8);
        ex.clk_add(2);
        if FETCH_TAB[ins as usize] != 0 {
            ex.clk_add(1);
        }
    } else {
        // Per-byte LOAD path. ts:1702-1714
        let ins = ex.load(reg_pc0);
        ex.clk_add(1);
        let op1 = ex.load(reg_pc0.wrapping_add(1));
        p0 = ins;
        p1 = op1;
        let mut p2v = op1 as u16;
        ex.clk_add(1);
        if FETCH_TAB[ins as usize] != 0 {
            let op2 = ex.load(reg_pc0.wrapping_add(2));
            p2v |= (op2 as u16) << 8;
            ex.clk_add(1);
        }
        p2 = p2v;
    }

    // Trap-skip label (6510core.c:2451). ts:1721
    ex.set_last_opcode(p0 as u32);

    // Tracing hook. ts:1724
    {
        let pc = ex.core.reg_pc;
        let clk = ex.core.clk;
        ex.bus.debug_drive(pc, clk, p0, p1, ((p2 >> 8) & 0xff) as u8);
    }

    ex.set_last_addr(ex.core.reg_pc);

    // Opcode switch (6510core.c:2454-3467). VERBATIM — each case 1:1. ts:1733-2031
    match p0 {
        0x00 => ex.brk(), // BRK
        0x01 => {
            let v = ex.load_ind_x(p1);
            ex.ora(v, 1, 2);
        } // ORA ($nn,X)
        0x02 => {
            // JAM / TRAP_OPCODE — JAM_02 mutates state + sets jam_result. ts:1736
            ex.jam_02();
        }
        0x22 | 0x52 | 0x62 | 0x72 | 0x92 | 0xb2 | 0xd2 | 0xf2 | 0x12 | 0x32 | 0x42 => {
            ex.core.is_jammed = true;
            ex.rewind_fetch_opcode();
            ex.jam_result = ex.host_jam();
        }

        0x03 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            let a = ex.load_zero_addr(p1.wrapping_add(ex.core.reg_x));
            ex.slo(a, 2, 2, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0x04 | 0x44 | 0x64 => ex.noop(1, 2),
        0x05 => {
            let v = ex.load_zero(p1);
            ex.ora(v, 1, 2);
        }
        0x06 => ex.asl(p1 as u16, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw),
        0x07 => ex.slo(p1 as u16, 0, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw),
        0x08 => {
            // PHP — drive: rotate + byte-ready edge → overflow (6510core.c:2525-2533). ts:1751-1759
            ex.bus.rotate();
            if ex.bus.byte_ready() {
                ex.bus.byte_ready_edge_clear();
                ex.local_set_overflow(true);
            }
            ex.php();
        }
        0x09 => ex.ora(p1, 0, 2),
        0x0a => ex.asl_a(),
        0x0b | 0x2b => ex.anc(p1, 2),
        0x0c => ex.noop_abs(p2),
        0x0d => {
            let v = ex.load(p2);
            ex.ora(v, 1, 3);
        }
        0x0e => ex.asl(p2, 3, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw),
        0x0f => ex.slo(p2, 0, 3, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw),

        0x10 => ex.branch(!ex.local_sign(), p1),
        0x11 => {
            let v = ex.load_ind_y(p1);
            ex.ora(v, 1, 2);
        }
        0x13 => ex.slo_ind_y(p1),
        0x14 | 0x34 | 0x54 | 0x74 | 0xd4 | 0xf4 => {
            ex.noop_load_zero_x(p1);
            ex.noop(CLK_NOOP_ZERO_X as i64, 2);
        }
        0x15 => {
            let v = ex.load_zero_x(p1);
            ex.ora(v, CLK_ZERO_I2 as i64, 2);
        }
        0x16 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            ex.asl(p1.wrapping_add(ex.core.reg_x) as u16, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0x17 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            ex.slo(p1.wrapping_add(ex.core.reg_x) as u16, 0, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0x18 => ex.clc(),
        0x19 => {
            let v = ex.load_abs_y(p2);
            ex.ora(v, 1, 3);
        }
        0x1a | 0x3a | 0x5a | 0x7a | 0xda | 0xfa => ex.noop_imm(1),
        0x1b => ex.slo(p2, 0, 3, LoadFn::AbsYRmw, StoreFn::AbsYRmw, DummyFn::AbsYRmw),
        0x1c | 0x3c | 0x5c | 0x7c | 0xdc | 0xfc => ex.noop_abs_x(p2),
        0x1d => {
            let v = ex.load_abs_x(p2);
            ex.ora(v, 1, 3);
        }
        0x1e => ex.asl(p2, 3, LoadFn::AbsXRmw, StoreFn::AbsXRmw, DummyFn::AbsXRmw),
        0x1f => ex.slo(p2, 0, 3, LoadFn::AbsXRmw, StoreFn::AbsXRmw, DummyFn::AbsXRmw),

        0x20 => ex.jsr(p1),
        0x21 => {
            let v = ex.load_ind_x(p1);
            ex.and(v, 1, 2);
        }
        0x23 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            let a = ex.load_zero_addr(p1.wrapping_add(ex.core.reg_x));
            ex.rla(a, 2, 2, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0x24 => {
            let v = ex.load_zero(p1);
            ex.bit(v, 2);
        }
        0x25 => {
            let v = ex.load_zero(p1);
            ex.and(v, 1, 2);
        }
        0x26 => ex.rol(p1 as u16, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw),
        0x27 => ex.rla(p1 as u16, 0, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw),
        0x28 => ex.plp(),
        0x29 => ex.and(p1, 0, 2),
        0x2a => ex.rol_a(),
        0x2c => {
            let v = ex.load(p2);
            ex.bit(v, 3);
        }
        0x2d => {
            let v = ex.load(p2);
            ex.and(v, 1, 3);
        }
        0x2e => ex.rol(p2, 3, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw),
        0x2f => ex.rla(p2, 0, 3, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw),

        0x30 => ex.branch(ex.local_sign(), p1),
        0x31 => {
            let v = ex.load_ind_y(p1);
            ex.and(v, 1, 2);
        }
        0x33 => ex.rla_ind_y(p1),
        0x35 => {
            let v = ex.load_zero_x(p1);
            ex.and(v, CLK_ZERO_I2 as i64, 2);
        }
        0x36 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            ex.rol(p1.wrapping_add(ex.core.reg_x) as u16, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0x37 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            ex.rla(p1.wrapping_add(ex.core.reg_x) as u16, 0, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0x38 => ex.sec(),
        0x39 => {
            let v = ex.load_abs_y(p2);
            ex.and(v, 1, 3);
        }
        0x3b => ex.rla(p2, 0, 3, LoadFn::AbsYRmw, StoreFn::AbsYRmw, DummyFn::AbsYRmw),
        0x3d => {
            let v = ex.load_abs_x(p2);
            ex.and(v, 1, 3);
        }
        0x3e => ex.rol(p2, 3, LoadFn::AbsXRmw, StoreFn::AbsXRmw, DummyFn::AbsXRmw),
        0x3f => ex.rla(p2, 0, 3, LoadFn::AbsXRmw, StoreFn::AbsXRmw, DummyFn::AbsXRmw),

        0x40 => ex.rti(),
        0x41 => {
            let v = ex.load_ind_x(p1);
            ex.eor(v, 1, 2);
        }
        0x43 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            let a = ex.load_zero_addr(p1.wrapping_add(ex.core.reg_x));
            ex.sre(a, 2, 2, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0x45 => {
            let v = ex.load_zero(p1);
            ex.eor(v, 1, 2);
        }
        0x46 => ex.lsr(p1 as u16, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw),
        0x47 => ex.sre(p1 as u16, 0, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw),
        0x48 => ex.pha(),
        0x49 => ex.eor(p1, 0, 2),
        0x4a => ex.lsr_a(),
        0x4b => ex.asr(p1, 2),
        0x4c => ex.jmp(p2),
        0x4d => {
            let v = ex.load(p2);
            ex.eor(v, 1, 3);
        }
        0x4e => ex.lsr(p2, 3, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw),
        0x4f => ex.sre(p2, 0, 3, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw),

        0x50 => {
            // BVC — drive: pre-branch rotate + byte_ready (6510core.c:2812-2821). ts:1836-1846
            ex.clk_add(-1);
            ex.bus.rotate();
            if ex.bus.byte_ready() {
                ex.bus.byte_ready_edge_clear();
                ex.local_set_overflow(true);
            }
            ex.clk_add(1);
            ex.branch(!ex.local_overflow(), p1);
        }
        0x51 => {
            let v = ex.load_ind_y(p1);
            ex.eor(v, 1, 2);
        }
        0x53 => ex.sre_ind_y(p1),
        0x55 => {
            let v = ex.load_zero_x(p1);
            ex.eor(v, CLK_ZERO_I2 as i64, 2);
        }
        0x56 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            ex.lsr(p1.wrapping_add(ex.core.reg_x) as u16, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0x57 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            ex.sre(p1.wrapping_add(ex.core.reg_x) as u16, 0, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0x58 => ex.cli(),
        0x59 => {
            let v = ex.load_abs_y(p2);
            ex.eor(v, 1, 3);
        }
        0x5b => ex.sre(p2, 0, 3, LoadFn::AbsYRmw, StoreFn::AbsYRmw, DummyFn::AbsYRmw),
        0x5d => {
            let v = ex.load_abs_x(p2);
            ex.eor(v, 1, 3);
        }
        0x5e => ex.lsr(p2, 3, LoadFn::AbsXRmw, StoreFn::AbsXRmw, DummyFn::AbsXRmw),
        0x5f => ex.sre(p2, 0, 3, LoadFn::AbsXRmw, StoreFn::AbsXRmw, DummyFn::AbsXRmw),

        0x60 => ex.rts(),
        0x61 => {
            let v = ex.load_ind_x(p1);
            ex.adc(v, 1, 2);
        }
        0x63 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            let a = ex.load_zero_addr(p1.wrapping_add(ex.core.reg_x));
            ex.rra(a, 2, 2, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0x65 => {
            let v = ex.load_zero(p1);
            ex.adc(v, 1, 2);
        }
        0x66 => ex.ror(p1 as u16, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw),
        0x67 => ex.rra(p1 as u16, 0, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw),
        0x68 => ex.pla(),
        0x69 => ex.adc(p1, 0, 2),
        0x6a => ex.ror_a(),
        0x6b => ex.arr(p1, 2),
        0x6c => ex.jmp_ind(p2),
        0x6d => {
            let v = ex.load(p2);
            ex.adc(v, 1, 3);
        }
        0x6e => ex.ror(p2, 3, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw),
        0x6f => ex.rra(p2, 0, 3, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw),

        0x70 => {
            // BVS — drive: pre-branch rotate + byte_ready (6510core.c:2931-2940). ts:1877-1887
            ex.clk_add(-1);
            ex.bus.rotate();
            if ex.bus.byte_ready() {
                ex.bus.byte_ready_edge_clear();
                ex.local_set_overflow(true);
            }
            ex.clk_add(1);
            ex.branch(ex.local_overflow(), p1);
        }
        0x71 => {
            let v = ex.load_ind_y(p1);
            ex.adc(v, 1, 2);
        }
        0x73 => ex.rra_ind_y(p1),
        0x75 => {
            let v = ex.load_zero_x(p1);
            ex.adc(v, CLK_ZERO_I2 as i64, 2);
        }
        0x76 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            ex.ror(p1.wrapping_add(ex.core.reg_x) as u16, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0x77 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            ex.rra(p1.wrapping_add(ex.core.reg_x) as u16, 0, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0x78 => ex.sei(),
        0x79 => {
            let v = ex.load_abs_y(p2);
            ex.adc(v, 1, 3);
        }
        0x7b => ex.rra(p2, 0, 3, LoadFn::AbsYRmw, StoreFn::AbsYRmw, DummyFn::AbsYRmw),
        0x7d => {
            let v = ex.load_abs_x(p2);
            ex.adc(v, 1, 3);
        }
        0x7e => ex.ror(p2, 3, LoadFn::AbsXRmw, StoreFn::AbsXRmw, DummyFn::AbsXRmw),
        0x7f => ex.rra(p2, 0, 3, LoadFn::AbsXRmw, StoreFn::AbsXRmw, DummyFn::AbsXRmw),

        0x80 | 0x82 | 0x89 | 0xc2 | 0xe2 => ex.noop_imm(2),
        0x81 => {
            ex.load_zero_dummy(p1);
            let a = ex.load_zero_addr(p1.wrapping_add(ex.core.reg_x));
            ex.sta(a, 3, 1, 2, StoreFn::Abs);
        }
        0x83 => {
            ex.load_zero_dummy(p1);
            let a = ex.load_zero_addr(p1.wrapping_add(ex.core.reg_x));
            ex.sax(a, 3, 1, 2);
        }
        0x84 => ex.sty_zero(p1, 1, 2),
        0x85 => ex.sta_zero(p1, 1, 2),
        0x86 => ex.stx_zero(p1, 1, 2),
        0x87 => ex.sax_zero(p1, 1, 2),
        0x88 => ex.dey(),
        0x8a => ex.txa(),
        0x8b => ex.ane(p1, 2),
        0x8c => ex.sty(p2, 1, 3),
        0x8d => ex.sta(p2, 0, 1, 3, StoreFn::Abs),
        0x8e => ex.stx(p2, 1, 3),
        0x8f => ex.sax(p2, 0, 1, 3),

        0x90 => ex.branch(!ex.local_carry(), p1),
        0x91 => ex.sta_ind_y(p1),
        0x93 => ex.sha_ind_y(p1),
        0x94 => {
            ex.load_zero_dummy(p1);
            ex.sty_zero(p1.wrapping_add(ex.core.reg_x), CLK_ZERO_I_STORE as i64, 2);
        }
        0x95 => {
            ex.load_zero_dummy(p1);
            ex.sta_zero(p1.wrapping_add(ex.core.reg_x), CLK_ZERO_I_STORE as i64, 2);
        }
        0x96 => {
            ex.load_zero_dummy(p1);
            ex.stx_zero(p1.wrapping_add(ex.core.reg_y), CLK_ZERO_I_STORE as i64, 2);
        }
        0x97 => {
            ex.load_zero_dummy(p1);
            ex.sax(p1.wrapping_add(ex.core.reg_y) as u16, 0, CLK_ZERO_I_STORE as i64, 2);
        }
        0x98 => ex.tya(),
        0x99 => ex.sta(p2, 0, CLK_ABS_I_STORE2 as i64, 3, StoreFn::AbsY), // STORE_ABS_Y. ts:1927
        0x9a => ex.txs(),
        0x9b => ex.shs_abs_y(p2),
        0x9c => ex.shy_abs_x(p2),
        0x9d => ex.sta(p2, 0, CLK_ABS_I_STORE2 as i64, 3, StoreFn::AbsX), // STORE_ABS_X. ts:1931
        0x9e => ex.shx_abs_y(p2),
        0x9f => ex.sha_abs_y(p2),

        0xa0 => ex.ldy(p1, 0, 2),
        0xa1 => {
            let v = ex.load_ind_x(p1);
            ex.lda(v, 1, 2);
        }
        0xa2 => ex.ldx(p1, 0, 2),
        0xa3 => {
            let v = ex.load_ind_x(p1);
            ex.lax(v, 1, 2);
        }
        0xa4 => {
            let v = ex.load_zero(p1);
            ex.ldy(v, 1, 2);
        }
        0xa5 => {
            let v = ex.load_zero(p1);
            ex.lda(v, 1, 2);
        }
        0xa6 => {
            let v = ex.load_zero(p1);
            ex.ldx(v, 1, 2);
        }
        0xa7 => {
            let v = ex.load_zero(p1);
            ex.lax(v, 1, 2);
        }
        0xa8 => ex.tay(),
        0xa9 => ex.lda(p1, 0, 2),
        0xaa => ex.tax(),
        0xab => ex.lxa(p1, 2),
        0xac => {
            let v = ex.load(p2);
            ex.ldy(v, 1, 3);
        }
        0xad => {
            let v = ex.load(p2);
            ex.lda(v, 1, 3);
        }
        0xae => {
            let v = ex.load(p2);
            ex.ldx(v, 1, 3);
        }
        0xaf => {
            let v = ex.load(p2);
            ex.lax(v, 1, 3);
        }

        0xb0 => ex.branch(ex.local_carry(), p1),
        0xb1 => {
            let v = ex.load_ind_y_bank(p1);
            ex.lda(v, 1, 2);
        }
        0xb3 => {
            let v = ex.load_ind_y(p1);
            ex.lax(v, 1, 2);
        }
        0xb4 => {
            let v = ex.load_zero_x(p1);
            ex.ldy(v, CLK_ZERO_I2 as i64, 2);
        }
        0xb5 => {
            let v = ex.load_zero_x(p1);
            ex.lda(v, CLK_ZERO_I2 as i64, 2);
        }
        0xb6 => {
            let v = ex.load_zero_y(p1);
            ex.ldx(v, CLK_ZERO_I2 as i64, 2);
        }
        0xb7 => {
            let v = ex.load_zero_y(p1);
            ex.lax(v, CLK_ZERO_I2 as i64, 2);
        }
        0xb8 => ex.clv(),
        0xb9 => {
            let v = ex.load_abs_y(p2);
            ex.lda(v, 1, 3);
        }
        0xba => ex.tsx(),
        0xbb => {
            let v = ex.load_abs_y(p2);
            ex.las(v, 1, 3);
        }
        0xbc => {
            let v = ex.load_abs_x(p2);
            ex.ldy(v, 1, 3);
        }
        0xbd => {
            let v = ex.load_abs_x(p2);
            ex.lda(v, 1, 3);
        }
        0xbe => {
            let v = ex.load_abs_y(p2);
            ex.ldx(v, 1, 3);
        }
        0xbf => {
            let v = ex.load_abs_y(p2);
            ex.lax(v, 1, 3);
        }

        0xc0 => ex.cpy(p1, 0, 2),
        0xc1 => {
            let v = ex.load_ind_x(p1);
            ex.cmp(v, 1, 2);
        }
        0xc3 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            let a = ex.load_zero_addr(p1.wrapping_add(ex.core.reg_x));
            ex.dcp(a, 2, 2, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0xc4 => {
            let v = ex.load_zero(p1);
            ex.cpy(v, 1, 2);
        }
        0xc5 => {
            let v = ex.load_zero(p1);
            ex.cmp(v, 1, 2);
        }
        0xc6 => ex.dec(p1 as u16, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw),
        0xc7 => ex.dcp(p1 as u16, 0, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw),
        0xc8 => ex.iny(),
        0xc9 => ex.cmp(p1, 0, 2),
        0xca => ex.dex(),
        0xcb => ex.sbx(p1, 2),
        0xcc => {
            let v = ex.load(p2);
            ex.cpy(v, 1, 3);
        }
        0xcd => {
            let v = ex.load(p2);
            ex.cmp(v, 1, 3);
        }
        0xce => ex.dec(p2, 3, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw),
        0xcf => ex.dcp(p2, 0, 3, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw),

        0xd0 => ex.branch(!ex.local_zero(), p1),
        0xd1 => {
            let v = ex.load_ind_y(p1);
            ex.cmp(v, 1, 2);
        }
        0xd3 => ex.dcp_ind_y(p1),
        0xd5 => {
            let v = ex.load_zero_x(p1);
            ex.cmp(v, CLK_ZERO_I2 as i64, 2);
        }
        0xd6 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            ex.dec(p1.wrapping_add(ex.core.reg_x) as u16, 2, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0xd7 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            ex.dcp(p1.wrapping_add(ex.core.reg_x) as u16, 0, 2, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0xd8 => ex.cld(),
        0xd9 => {
            let v = ex.load_abs_y(p2);
            ex.cmp(v, 1, 3);
        }
        0xdb => ex.dcp(p2, 0, 3, LoadFn::AbsYRmw, StoreFn::AbsYRmw, DummyFn::AbsYRmw),
        0xdd => {
            let v = ex.load_abs_x(p2);
            ex.cmp(v, 1, 3);
        }
        0xde => ex.dec(p2, 3, LoadFn::AbsXRmw, StoreFn::AbsXRmw, DummyFn::AbsXRmw),
        0xdf => ex.dcp(p2, 0, 3, LoadFn::AbsXRmw, StoreFn::AbsXRmw, DummyFn::AbsXRmw),

        0xe0 => ex.cpx(p1, 0, 2),
        0xe1 => {
            let v = ex.load_ind_x(p1);
            ex.sbc(v, 1, 2);
        }
        0xe3 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            let a = ex.load_zero_addr(p1.wrapping_add(ex.core.reg_x));
            ex.isb(a, 2, 2, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0xe4 => {
            let v = ex.load_zero(p1);
            ex.cpx(v, 1, 2);
        }
        0xe5 => {
            let v = ex.load_zero(p1);
            ex.sbc(v, 1, 2);
        }
        0xe6 => ex.inc(p1 as u16, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw),
        0xe7 => ex.isb(p1 as u16, 0, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw),
        0xe8 => ex.inx(),
        0xe9 => ex.sbc(p1, 0, 2),
        0xea => ex.noop_imm(1), // NOP
        0xeb => ex.sbc(p1, 0, 2), // USBC = SBC
        0xec => {
            let v = ex.load(p2);
            ex.cpx(v, 1, 3);
        }
        0xed => {
            let v = ex.load(p2);
            ex.sbc(v, 1, 3);
        }
        0xee => ex.inc(p2, 3, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw),
        0xef => ex.isb(p2, 0, 3, LoadFn::Abs, StoreFn::Abs, DummyFn::AbsRmw),

        0xf0 => ex.branch(ex.local_zero(), p1),
        0xf1 => {
            let v = ex.load_ind_y(p1);
            ex.sbc(v, 1, 2);
        }
        0xf3 => ex.isb_ind_y(p1),
        0xf5 => {
            let v = ex.load_zero_x(p1);
            ex.sbc(v, CLK_ZERO_I2 as i64, 2);
        }
        0xf6 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            ex.inc(p1.wrapping_add(ex.core.reg_x) as u16, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0xf7 => {
            ex.load_zero_dummy(p1);
            ex.clk_add_dummy(1);
            ex.isb(p1.wrapping_add(ex.core.reg_x) as u16, 0, 2, LoadFn::Zero, StoreFn::Abs, DummyFn::AbsRmw);
        }
        0xf8 => ex.sed(),
        0xf9 => {
            let v = ex.load_abs_y(p2);
            ex.sbc(v, 1, 3);
        }
        0xfb => ex.isb(p2, 0, 3, LoadFn::AbsYRmw, StoreFn::AbsYRmw, DummyFn::AbsYRmw),
        0xfd => {
            let v = ex.load_abs_x(p2);
            ex.sbc(v, 1, 3);
        }
        0xfe => ex.inc(p2, 3, LoadFn::AbsXRmw, StoreFn::AbsXRmw, DummyFn::AbsXRmw),
        0xff => ex.isb(p2, 0, 3, LoadFn::AbsXRmw, StoreFn::AbsXRmw, DummyFn::AbsXRmw),
    }

    ex.jam_result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trivial flat-RAM bus with no rotation / no checkpoints. Used to smoke
    /// the core's register + clk accounting deterministically.
    struct RamBus {
        ram: [u8; 0x10000],
        rotates: u32,
    }
    impl RamBus {
        fn new() -> Self {
            RamBus {
                ram: [0u8; 0x10000],
                rotates: 0,
            }
        }
    }
    impl DriveCore6510Bus for RamBus {
        fn read(&mut self, addr: u16) -> u8 {
            self.ram[addr as usize]
        }
        fn write(&mut self, addr: u16, value: u8) {
            self.ram[addr as usize] = value;
        }
        fn rotate(&mut self) {
            self.rotates += 1;
        }
        fn byte_ready(&mut self) -> bool {
            false
        }
        fn byte_ready_edge_clear(&mut self) {}
    }

    #[test]
    fn smoke_lda_imm_sta_abs_branch() {
        let mut bus = RamBus::new();
        let mut int = IntStatus::default();
        let mut core = DriveCore6510::new();

        // Program at $0400:
        //   $0400 A9 42     LDA #$42
        //   $0402 8D 00 05  STA $0500
        //   $0405 A2 00     LDX #$00   (Z set)
        //   $0407 D0 02     BNE +2     (not taken, Z set)
        //   $0409 E8        INX
        //   $040A EA        NOP
        let prog: &[(u16, u8)] = &[
            (0x0400, 0xA9),
            (0x0401, 0x42),
            (0x0402, 0x8D),
            (0x0403, 0x00),
            (0x0404, 0x05),
            (0x0405, 0xA2),
            (0x0406, 0x00),
            (0x0407, 0xD0),
            (0x0408, 0x02),
            (0x0409, 0xE8),
            (0x040A, 0xEA),
        ];
        for &(a, b) in prog {
            bus.ram[a as usize] = b;
        }
        core.reg_pc = 0x0400;
        core.reg_sp = 0xff;
        let clk0 = core.clk;

        // LDA #$42
        let r = drive_6510core_execute(&mut core, &mut bus, &mut int);
        assert_eq!(r, JAM_NONE);
        assert_eq!(core.reg_a, 0x42, "A loaded");
        assert_eq!(core.reg_pc, 0x0402, "PC advanced past 2-byte LDA");
        // imm LDA: FETCH_OPCODE adds 2 (no 3rd byte) → 2 cycles.
        assert_eq!(core.clk - clk0, 2, "LDA #imm = 2 cycles");

        // STA $0500
        let clk1 = core.clk;
        drive_6510core_execute(&mut core, &mut bus, &mut int);
        assert_eq!(bus.ram[0x0500], 0x42, "STA wrote A to $0500");
        assert_eq!(core.reg_pc, 0x0405, "PC past 3-byte STA");
        // STA abs: fetch 3 (3-byte) + STORE_ABS inc=1 → 4 cycles.
        assert_eq!(core.clk - clk1, 4, "STA abs = 4 cycles");

        // LDX #$00 → Z set, X=0
        drive_6510core_execute(&mut core, &mut bus, &mut int);
        assert_eq!(core.reg_x, 0x00, "X loaded 0");
        assert_eq!(core.flag_z, 0, "Z set (flag_z == 0) after LDX #0");
        assert_eq!(core.reg_pc, 0x0407, "PC past LDX");

        // BNE +2 — Z is set so branch NOT taken; PC just advances 2.
        let clk_b = core.clk;
        drive_6510core_execute(&mut core, &mut bus, &mut int);
        assert_eq!(core.reg_pc, 0x0409, "BNE not taken, PC = $0409");
        // Not-taken branch: fetch 2 cycles only.
        assert_eq!(core.clk - clk_b, 2, "untaken branch = 2 cycles");

        // INX → X=1
        drive_6510core_execute(&mut core, &mut bus, &mut int);
        assert_eq!(core.reg_x, 0x01, "INX → 1");
        assert_ne!(core.flag_z, 0, "Z clear after INX→1");
        assert_eq!(core.reg_pc, 0x040A, "PC past INX");

        // NOP
        drive_6510core_execute(&mut core, &mut bus, &mut int);
        assert_eq!(core.reg_pc, 0x040B, "PC past NOP");
    }

    #[test]
    fn smoke_taken_branch_crosses_page() {
        // BNE that IS taken and crosses a page boundary costs an extra cycle.
        let mut bus = RamBus::new();
        let mut int = IntStatus::default();
        let mut core = DriveCore6510::new();
        // $04FE D0 7E  BNE +$7E → dest = $0500 + ... actually $04FE+2 = $0500,
        // +0x7E = $057E (no page cross from $0500). Use a backward target to
        // force a page cross instead.
        //   $0500 D0 FB  BNE -5  (Z clear) → dest = $0502 - 5 = $04FD (page cross)
        bus.ram[0x0500] = 0xD0;
        bus.ram[0x0501] = 0xFB;
        core.reg_pc = 0x0500;
        core.reg_sp = 0xff;
        core.flag_z = 1; // Z clear → BNE taken.
        let clk0 = core.clk;
        drive_6510core_execute(&mut core, &mut bus, &mut int);
        assert_eq!(core.reg_pc, 0x04FD, "taken BNE landed at $04FD");
        // fetch 2 + CLK_BRANCH2 (1) + page-cross CLK_BRANCH2 (1) = 4.
        assert_eq!(core.clk - clk0, 4, "taken page-crossing branch = 4 cycles");
    }
}
