//! VERBATIM port of the C64 (x64sc) main 6510 CPU core.
//!
//! PORT OF (PRIMARY, 1:1):
//!   vice/src/6510dtvcore.c        — the cycle-stepped ("SC") core. Despite the
//!                                   file name, x64sc's maincpu #includes THIS
//!                                   core (NOT 6510core.c) via the chain
//!                                   c64cpusc.c -> mainc64cpu.c -> 6510dtvcore.c.
//!                                   We port the NON-DTV, NON-DRIVE_CPU path
//!                                   (i.e. `#undef C64DTV`, `#undef DRIVE_CPU`),
//!                                   which IS the standard 6510 cycle-exact core.
//!   vice/src/mainc64cpu.c         — the maincpu wrapper: check_ba /
//!                                   maincpu_steal_cycles (the SH*/CLI/ANE/LXA
//!                                   ENABLES_IRQ-on-steal + SEI delay-suppress),
//!                                   interrupt_delay(), the LOAD/STORE/PUSH/PULL
//!                                   macros, interrupt_check_irq_delay /
//!                                   interrupt_check_nmi_delay (delay-cycle
//!                                   counters, NOT the drive's clk-compare).
//!   vice/src/c64/c64cpusc.c       — CLK_INC() (interrupt_delay + maincpu_clk++ +
//!                                   vicii_cycle()), FETCH_OPCODE, SET_OPCODE,
//!                                   REWIND_FETCH_OPCODE (no-op for SC), SKIP_CYCLE=0,
//!                                   OPCODE_UPDATE_IN_FETCH.
//! CROSS-CHECK:
//!   vice/src/interrupt.h / interrupt.c — interrupt_cpu_status_s, interrupt_set_irq/nmi,
//!                                   interrupt_ack_*, interrupt_fixup_int_clk, the
//!                                   *_cpu_status_reset/init/new bodies.
//!   vice/src/6510core.h / mos6510.h — OPINFO_* masks/accessors, P_* flag bits.
//!   crates/trx64-core/src/drive_6510core.rs — the proven Rust template (the
//!                                   DRIVE variant of the SAME 6510 ISA); reused
//!                                   for the ALU ops, structure, and convention.
//!
//! WHY A SEPARATE CYCLE-STEPPED CORE: the C64 SC core threads `check_ba()` (the
//! VIC BA cycle-steal) and `CLK_INC()` (the per-cycle Phi1/Phi2 VIC tick +
//! interrupt_delay) into EVERY bus access. The illegal opcodes are fully
//! cycle-stepped (each access has its own CLK_INC), the SH*/SHX/SHY stores carry
//! the page-cross target-high-byte corruption + mispaged dummy read
//! (`SET_ABS_SH_I`), taken branches emit the proper dummy reads, and the
//! interrupt model is the real OPINFO + delay-cycle-counter model — exactly the
//! gaps the audit found in cpu.rs (the microcode-pattern engine).
//!
//! Line-correspondence convention: every helper and opcode case is tagged with
//! `// dtv:<N>` giving the 6510dtvcore.c line, `// m64:<N>` for mainc64cpu.c,
//! `// sc:<N>` for c64cpusc.c, `// int.h:<N>` for interrupt.h. A reviewer can
//! diff line-by-line against those files by the tags.
//!
//! NOT wired into lib.rs/the machine yet — that is the next step. The C64
//! machine (lib.rs) will implement `C64Core6510Bus` over its existing bus + VIC
//! + CIAs and drive `c64_6510core_execute(&mut core, &mut bus, &mut int)` once
//! per instruction while `core.clk < stop_clk`.

#![allow(clippy::needless_range_loop)]
#![allow(clippy::too_many_arguments)]
#![allow(non_snake_case)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::collapsible_if)]
#![allow(dead_code)]
// The following are deliberate VICE-shape choices kept for line-by-line parity
// with 6510dtvcore.c — clippy's rewrites would obscure the C correspondence.
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::no_effect)]
#![allow(clippy::needless_late_init)]
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::enum_variant_names)]
#![allow(clippy::manual_range_contains)]
#![allow(clippy::identity_op)]
#![allow(clippy::precedence)]

// =============================================================================
// SECTION A — Constants.
// PORT OF: vice/src/mos6510.h:52-59 (P_* flag bits).
// =============================================================================
const P_SIGN: u8 = 0x80;
const P_OVERFLOW: u8 = 0x40;
const P_UNUSED: u8 = 0x20;
const P_BREAK: u8 = 0x10;
const P_DECIMAL: u8 = 0x08;
const P_INTERRUPT: u8 = 0x04;
const P_ZERO: u8 = 0x02;
const P_CARRY: u8 = 0x01;

// PORT OF: vice/src/6510core.h:31-34 (opinfo masks).
const OPINFO_DELAYS_INTERRUPT_MSK: u32 = 1 << 8;
const OPINFO_DISABLES_IRQ_MSK: u32 = 1 << 9;
const OPINFO_ENABLES_IRQ_MSK: u32 = 1 << 10;
// OPINFO_NUMBER(opinfo) = low byte (the opcode of the last-executed instruction).
// PORT OF: vice/src/6510core.h (OPINFO_NUMBER).
const OPINFO_NUMBER_MSK: u32 = 0xff;

// PORT OF: vice/src/6510core.h:36-55 (opinfo accessors).
#[inline]
fn opinfo_number(opinfo: u32) -> u32 {
    opinfo & OPINFO_NUMBER_MSK
}
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

// PORT OF: vice/src/interrupt.h:39 (INTERRUPT_DELAY).
pub const INTERRUPT_DELAY: u64 = 2;
// PORT OF: vice/src/interrupt.h:44-53 (enum cpu_int).
pub const IK_NONE: u32 = 0;
pub const IK_NMI: u32 = 1 << 0;
pub const IK_IRQ: u32 = 1 << 1;
pub const IK_RESET: u32 = 1 << 2;
pub const IK_TRAP: u32 = 1 << 3;
pub const IK_MONITOR: u32 = 1 << 4;
pub const IK_DMA: u32 = 1 << 5;
pub const IK_IRQPEND: u32 = 1 << 6;

// PORT OF: vice/src/machine.h:188-192 — JAM reason codes.
pub const JAM_NONE: i32 = 0;
pub const JAM_RESET_CPU: i32 = 1;
pub const JAM_POWER_CYCLE: i32 = 2;
pub const JAM_MONITOR: i32 = 3;

// PORT OF: vice/src/traps.h — TRAP_OPCODE used by JAM_02(). dtv:1855
const TRAP_OPCODE: u8 = 0x02;

// PORT OF: vice/src/6510dtvcore.c:825-826 (ANE) / :1280-1281 (LXA).
const ANE_MAGIC: u8 = 0xef;
const ANE_RDY_MAGIC: u8 = 0xee & ANE_MAGIC; // = 0xee
const LXA_MAGIC: u8 = 0xee;
const LXA_RDY_MAGIC: u8 = 0xee;

// "irrelevant" sentinel for an inactive irq_clk / nmi_clk / irq_pending_clk
// (interrupt.c uses CLOCK_MAX). A source only asserts when the clk comparison
// can be satisfied, so a MAX sentinel never fires while no source asserts.
pub const CLOCK_MAX: u64 = u64::MAX;

/// Number of independent interrupt sources the C64 int status multiplexes
/// (interrupt.h pending_int[]). The stock no-cart C64 wires four: VIC-II (IRQ),
/// CIA1 (IRQ), CIA2 (NMI), and the RESTORE key (NMI). The host registers sources
/// at init by `int_num`; the named indices below are the registration order
/// (VICE registers vicii first, then the CIAs, then restore — but only the count
/// and per-source independence matter, since `set_irq`/`set_nmi` key off
/// `int_num` and the global edge is `nirq`/`nnmi` aggregated). Index = int_num.
pub const C64_NUM_INT_SOURCES: usize = 4;

/// VIC-II raster/sprite IRQ source index (int_num).
pub const INT_SRC_VIC: usize = 0;
/// CIA1 timer/TOD/FLAG IRQ source index (int_num).
pub const INT_SRC_CIA1: usize = 1;
/// CIA2 timer/TOD/FLAG NMI source index (int_num).
pub const INT_SRC_CIA2: usize = 2;
/// RESTORE-key NMI source index (int_num). Inert in the headless machine (no
/// keyboard RESTORE wired into the NMI line), reserved for completeness.
pub const INT_SRC_RESTORE: usize = 3;

// SC: REWIND_FETCH_OPCODE is a NO-OP for x64sc (c64cpusc.c:42 `/*clock-=2*/`).
// We model it as a no-op accordingly.

// =============================================================================
// SECTION B — interrupt_cpu_status mirror.
//
// PORT OF: vice/src/interrupt.h:55-128 (struct interrupt_cpu_status_s) — the
// subset the SC maincpu uses. The maincpu model (UNLIKE the drive's clk-compare)
// drives the IRQ/NMI decision off the `irq_delay_cycles`/`nmi_delay_cycles`
// counters, which `interrupt_delay()` (mainc64cpu.c:97-110, called per CLK_INC)
// increments once per cycle while the source is asserted.
// =============================================================================

/// Interrupt status mirror. PORT OF: vice/src/interrupt.h:55-128.
#[derive(Clone, Debug)]
pub struct IntStatus {
    /// interrupt.h:57 — number of registered interrupt sources.
    pub num_ints: u32,
    /// interrupt.h:61 — per-source asserted-line bitmask (pending_int[]).
    pub pending_int: [u32; C64_NUM_INT_SOURCES],
    /// interrupt.h:67 — how many sources currently assert IRQ.
    pub nirq: i32,
    /// interrupt.h:70 — clk when the IRQ was first triggered.
    pub irq_clk: u64,
    /// interrupt.h:73 — how many sources currently assert NMI.
    pub nnmi: i32,
    /// interrupt.h:76 — clk when the NMI was first triggered.
    pub nmi_clk: u64,
    /// interrupt.h:81 — DMA-per-opcode count (for the steal fixup path).
    pub num_dma_per_opcode: u32,
    /// interrupt.h:82-83 — DMA bookkeeping for interrupt_fixup_int_clk.
    pub num_cycles_left: Vec<u64>,
    pub dma_start_clk: Vec<u64>,
    /// interrupt.h:86 — counter for delay between IRQ request and handler.
    pub irq_delay_cycles: u64,
    /// interrupt.h:87 — counter for delay between NMI request and handler.
    pub nmi_delay_cycles: u64,
    /// interrupt.h:111 — last_opcode_info word (LAST_OPCODE_INFO).
    pub last_opcode_info: u32,
    /// interrupt.h:114 — number of cycles stolen last time.
    pub num_last_stolen_cycles: u64,
    /// interrupt.h:117 — clk at which those cycles were stolen.
    pub last_stolen_cycles_clk: u64,
    /// interrupt.h:121 — clk where just-ACK'd IRQs may still fire. CLOCK_MAX = irrelevant.
    pub irq_pending_clk: u64,
    /// interrupt.h:123 — combined pending bitfield.
    pub global_pending_int: u32,
}

impl Default for IntStatus {
    fn default() -> Self {
        Self::new()
    }
}

impl IntStatus {
    /// PORT OF: interrupt_cpu_status_new (interrupt.c:97-100) lib_calloc — all
    /// zero — then interrupt_cpu_status_init (interrupt.c:45-52) +
    /// interrupt_cpu_status_reset (interrupt.c:54-81) bring it to the live state.
    pub fn new() -> Self {
        let mut cs = IntStatus {
            num_ints: C64_NUM_INT_SOURCES as u32,
            pending_int: [IK_NONE; C64_NUM_INT_SOURCES],
            nirq: 0,
            irq_clk: CLOCK_MAX,
            nnmi: 0,
            nmi_clk: CLOCK_MAX,
            num_dma_per_opcode: 0,
            num_cycles_left: Vec::new(),
            dma_start_clk: Vec::new(),
            irq_delay_cycles: 0,
            nmi_delay_cycles: 0,
            last_opcode_info: 0,
            num_last_stolen_cycles: 0,
            last_stolen_cycles_clk: 0,
            irq_pending_clk: CLOCK_MAX,
            global_pending_int: IK_NONE,
        };
        cs.cpu_status_reset();
        cs
    }

    /// PORT OF: vice/src/interrupt.c:54-81 interrupt_cpu_status_reset. Zeroes the
    /// dynamic state but preserves the registered sources; irq_clk/nmi_clk/
    /// irq_pending_clk all reset to CLOCK_MAX (memset 0 then the explicit
    /// assignments; we keep the *_clk sentinels at CLOCK_MAX as the live code
    /// expects). pending_int[] cleared.
    pub fn cpu_status_reset(&mut self) {
        for p in self.pending_int.iter_mut() {
            *p = 0;
        }
        self.nirq = 0;
        self.nnmi = 0;
        self.irq_clk = CLOCK_MAX;
        self.nmi_clk = CLOCK_MAX;
        self.num_last_stolen_cycles = 0;
        self.last_stolen_cycles_clk = 0;
        self.num_dma_per_opcode = 0;
        self.irq_delay_cycles = 0;
        self.nmi_delay_cycles = 0;
        self.global_pending_int = IK_NONE;
        self.irq_pending_clk = CLOCK_MAX;
    }

    /// PORT OF: vice/src/interrupt.h:141-196 interrupt_set_irq — the per-source
    /// IRQ-line setter the CIA/VIC `set_int` calls with the precise rclk. The
    /// `irq_clk`/`irq_delay_cycles` are stamped ONLY on the `nirq` 0→1 edge.
    /// On the final deassert `irq_pending_clk = cpu_clk + 3` arms the IK_IRQPEND
    /// tail. The fixup path (last_stolen_cycles_clk > cpu_clk) routes through
    /// interrupt_fixup_int_clk.
    #[inline]
    pub fn set_irq(&mut self, int_num: usize, value: bool, cpu_clk: u64) {
        if int_num >= self.num_ints as usize {
            return;
        }
        if value {
            if self.pending_int[int_num] & IK_IRQ == 0 {
                self.pending_int[int_num] |= IK_IRQ;
                if self.nirq == 0 {
                    self.global_pending_int |= IK_IRQ | IK_IRQPEND;
                    self.irq_pending_clk = CLOCK_MAX;
                    self.irq_delay_cycles = 0;
                    if self.last_stolen_cycles_clk <= cpu_clk {
                        self.irq_clk = cpu_clk;
                    } else {
                        let mut int_clk = self.irq_clk;
                        self.fixup_int_clk(cpu_clk, &mut int_clk);
                        self.irq_clk = int_clk;
                    }
                }
                self.nirq += 1;
            }
        } else if self.pending_int[int_num] & IK_IRQ != 0 {
            if self.nirq > 0 {
                self.pending_int[int_num] &= !IK_IRQ;
                self.nirq -= 1;
                if self.nirq == 0 {
                    self.global_pending_int &= !IK_IRQ;
                    self.irq_pending_clk = cpu_clk + 3;
                }
            }
        }
    }

    /// PORT OF: vice/src/interrupt.h:199-250 interrupt_set_nmi — edge-triggered
    /// NMI setter. nmi_clk/nmi_delay_cycles stamped on the 0→1 edge (and only
    /// when IK_NMI not already globally pending). The deassert path does NOT
    /// clear global IK_NMI (only interrupt_ack_nmi does).
    #[inline]
    pub fn set_nmi(&mut self, int_num: usize, value: bool, cpu_clk: u64) {
        if int_num >= self.num_ints as usize {
            return;
        }
        if value {
            if self.pending_int[int_num] & IK_NMI == 0 {
                if self.nnmi == 0 && (self.global_pending_int & IK_NMI) == 0 {
                    self.global_pending_int |= IK_NMI;
                    self.nmi_delay_cycles = 0;
                    if self.last_stolen_cycles_clk <= cpu_clk {
                        self.nmi_clk = cpu_clk;
                    } else {
                        let mut int_clk = self.nmi_clk;
                        self.fixup_int_clk(cpu_clk, &mut int_clk);
                        self.nmi_clk = int_clk;
                    }
                }
                self.nnmi += 1;
                self.pending_int[int_num] |= IK_NMI;
            }
        } else if self.pending_int[int_num] & IK_NMI != 0 {
            if self.nnmi > 0 {
                self.nnmi -= 1;
                self.pending_int[int_num] &= !IK_NMI;
                // #if 0 block in VICE — does NOT clear global IK_NMI here.
            }
        }
    }

    /// PORT OF: vice/src/interrupt.c:219-280 interrupt_fixup_int_clk — recomputes
    /// the int_clk when cycles were stolen mid-opcode (DMA / BA steal) before the
    /// IRQ/NMI request. Without stolen cycles num_dma_per_opcode==0 and this
    /// leaves int_clk = last_stolen_cycles_clk (the common SC case). The full
    /// interpolation honours the per-opcode DELAYS_INTERRUPT latency.
    fn fixup_int_clk(&self, cpu_clk: u64, int_clk: &mut u64) {
        let mut num_cycles_left: u64 = 0;
        let mut last_num_cycles_left: u64 = 0;
        let cycles_left_to_trigger_irq: u64 =
            if opinfo_delays_interrupt(self.last_opcode_info) != 0 { 2 } else { 1 };
        let mut last_start_clk: u64 = CLOCK_MAX;

        let mut num_dma = self.num_dma_per_opcode as usize;
        while num_dma != 0 {
            num_dma -= 1;
            num_cycles_left = self.num_cycles_left[num_dma];
            if self.dma_start_clk[num_dma].wrapping_sub(1) <= cpu_clk {
                break;
            }
            last_num_cycles_left = num_cycles_left;
            last_start_clk = self.dma_start_clk[num_dma];
        }
        // interpolate between two CPU cycles.
        if num_cycles_left.wrapping_sub(last_num_cycles_left)
            > last_start_clk.wrapping_sub(cpu_clk).wrapping_sub(1)
        {
            num_cycles_left =
                last_num_cycles_left + last_start_clk.wrapping_sub(cpu_clk).wrapping_sub(1);
        }

        *int_clk = self.last_stolen_cycles_clk;
        if self.num_dma_per_opcode > 0 && self.dma_start_clk[0] > cpu_clk {
            *int_clk = int_clk.wrapping_sub(self.dma_start_clk[0] - cpu_clk);
        }
        if num_cycles_left >= cycles_left_to_trigger_irq {
            *int_clk = int_clk.wrapping_sub(cycles_left_to_trigger_irq + 1);
        }
    }

    /// PORT OF: vice/src/interrupt.h:273-281 interrupt_ack_nmi.
    #[inline]
    pub fn interrupt_ack_nmi(&mut self) {
        self.global_pending_int &= !IK_NMI;
    }
    /// PORT OF: vice/src/interrupt.h:284-289 interrupt_ack_irq.
    #[inline]
    pub fn interrupt_ack_irq(&mut self) {
        self.global_pending_int &= !IK_IRQPEND;
        self.irq_pending_clk = CLOCK_MAX;
    }
    /// PORT OF: vice/src/interrupt.c:307-314 interrupt_ack_reset.
    #[inline]
    pub fn interrupt_ack_reset(&mut self) {
        self.global_pending_int &= !IK_RESET;
    }
}

// =============================================================================
// SECTION C — interrupt_delay + the delay-cycle checks.
//
// PORT OF: vice/src/mainc64cpu.c:97-110 interrupt_delay (called by CLK_INC
//   BEFORE the clk++), :663-685 interrupt_check_nmi_delay, :690-710
//   interrupt_check_irq_delay.
//
// The SC maincpu model: each CLK_INC dispatches alarms then, while a source's
// *_clk <= maincpu_clk, increments the matching *_delay_cycles counter. The
// check functions then compare the counter against INTERRUPT_DELAY (+1 if the
// last opcode DELAYS_INTERRUPT). This is the cycle-exact latency the audit's
// pattern engine approximated by opcode value.
// =============================================================================

/// PORT OF: vice/src/mainc64cpu.c:663-685 interrupt_check_nmi_delay.
/// BRK (0x00) defers NMI by one opcode → 0. A taken-no-page-cross branch
/// (DELAYS_INTERRUPT) bumps the threshold by one. Take the NMI iff
/// nmi_delay_cycles >= threshold.
#[inline]
fn interrupt_check_nmi_delay(cs: &IntStatus, _cpu_clk: u64) -> bool {
    let mut delay_cycles: u64 = INTERRUPT_DELAY;
    if opinfo_number(cs.last_opcode_info) == 0x00 {
        return false;
    }
    if opinfo_delays_interrupt(cs.last_opcode_info) != 0 {
        delay_cycles += 1;
    }
    cs.nmi_delay_cycles >= delay_cycles
}

/// PORT OF: vice/src/mainc64cpu.c:690-710 interrupt_check_irq_delay.
/// A taken-no-page-cross branch (DELAYS_INTERRUPT) bumps the threshold by one.
/// If irq_delay_cycles >= threshold: take the IRQ UNLESS the last opcode
/// ENABLES_IRQ (an I-clearing CLI/PLP), in which case defer one instruction by
/// latching IK_IRQPEND. This MUTATES the int status.
#[inline]
fn interrupt_check_irq_delay(cs: &mut IntStatus, _cpu_clk: u64) -> bool {
    let mut delay_cycles: u64 = INTERRUPT_DELAY;
    if opinfo_delays_interrupt(cs.last_opcode_info) != 0 {
        delay_cycles += 1;
    }
    if cs.irq_delay_cycles >= delay_cycles {
        if opinfo_enables_irq(cs.last_opcode_info) == 0 {
            return true;
        } else {
            cs.global_pending_int |= IK_IRQPEND;
        }
    }
    false
}

// =============================================================================
// SECTION D — host hook surface (the bus trait).
//
// The C64 SC core routes every access through LOAD/STORE which run check_ba()
// (mainc64cpu.c:359-369), and every cycle through CLK_INC() which runs
// interrupt_delay() + vicii_cycle() (c64cpusc.c:47-51). Those become methods on
// the bus trait so lib.rs implements them over its existing bus + VIC + CIAs.
// =============================================================================

/// Bus + VIC + interrupt hook surface the C64 SC core executes against.
///
/// lib.rs implements this over its existing C64 bus (RAM/ROM/IO via the
/// PLA banking), the VIC (vicii_cycle / BA steal), and the CIAs/alarm machinery.
/// Real vs DUMMY accesses are distinct methods because VICE routes them through
/// separate `_mem_read_tab_ptr` / `_mem_read_tab_ptr_dummy` tables; the dummy
/// variants exist so checkpoints / side-effects can distinguish a true access
/// from a wasted bus cycle. Default dummy impls delegate to the real ones.
pub trait C64Core6510Bus {
    /// PORT OF: mainc64cpu.c:359-363 mem_read_check_ba — the LOAD path. Runs
    /// check_ba() (cycle-steal) THEN the bus read. The implementor must call
    /// `self`-side check_ba via `check_ba()` below; the core calls check_ba
    /// explicitly at the macro sites that need it (FETCH_OPCODE), and LOAD/STORE
    /// run it internally. To keep the trait simple we let the implementor's
    /// `read`/`write` perform the check_ba() themselves OR the core calls
    /// `check_ba()` then `read_raw`. We expose BOTH the raw access and check_ba
    /// so the core reproduces the exact VICE ordering.
    fn read_raw(&mut self, addr: u16) -> u8;
    /// PORT OF: mainc64cpu.c:372-380 STORE (raw write tab). reu_dma($ff00) hook
    /// is folded into the implementor.
    fn write_raw(&mut self, addr: u16, value: u8);
    /// FETCH read raw — the opcode/operand byte fetch (c64cpusc.c FETCH_OPCODE,
    /// which reads through the separate `_mem_read_tab_ptr` fetch path). Distinct
    /// from `read_raw` so a tracing implementor can tag it as a FETCH access (which
    /// VICE / the conformance trace does NOT emit as a bus record, unlike data
    /// reads). Default delegates to `read_raw` (functionally identical bus access).
    #[inline]
    fn read_raw_fetch(&mut self, addr: u16) -> u8 {
        self.read_raw(addr)
    }
    /// DUMMY read raw (mainc64cpu.c:365-369 mem_read_check_ba_dummy minus check_ba).
    #[inline]
    fn read_raw_dummy(&mut self, addr: u16) -> u8 {
        self.read_raw(addr)
    }
    /// DUMMY write raw (mainc64cpu.c:382-388 STORE_DUMMY minus the $ff00 reu hook).
    #[inline]
    fn write_raw_dummy(&mut self, addr: u16, value: u8) {
        self.write_raw(addr, value)
    }

    /// PORT OF: mainc64cpu.c:194-208 check_ba — if any BA-low flag is set, steal
    /// cycles (maincpu_steal_cycles). Returns the number of cycles stolen (>=0);
    /// the core advances clk + ticks the VIC per stolen cycle inside the
    /// implementor (matching VICE, where steal advances maincpu_clk directly).
    /// The implementor must, per maincpu_steal_cycles (mainc64cpu.c:112-192),
    /// also apply the OPINFO ENABLES_IRQ steal-signal for SH*/CLI when
    /// `check_ba_low` is set (see `set_check_ba_low`). To do that it needs to
    /// read+mutate the last_opcode_info word: it is passed by &mut.
    fn check_ba(&mut self, last_opcode_info: &mut u32, check_ba_low: bool) -> u64;

    /// PORT OF: c64cpusc.c:47-51 CLK_INC()'s `vicii_cycle()` (the per-CLK
    /// Phi1/Phi2 VIC tick) + the `maincpu_ba_low_flags |= vicii_cycle()` update.
    /// Called once per CPU cycle, AFTER interrupt_delay() and the clk++ (which
    /// the core does itself). The implementor ticks the VIC one cycle and
    /// returns/records the new BA-low state for the next check_ba.
    fn vic_cycle(&mut self, clk: u64);

    /// PROCESS_ALARMS hook. PORT OF the alarm_context dispatch loop
    /// (dtv:1734-1736 / 1768-1770; DO_IRQBRK dtv:327-329). Advances the C64's
    /// CIA/VIC alarm machinery up to `clk`. The implementor ticks its CIAs here.
    #[inline]
    fn process_alarms(&mut self, _clk: u64) {}

    /// PORT OF: c64cpusc.c CLK_INC's interrupt_delay() alarm-dispatch half
    /// (mainc64cpu.c:99-101). Same as process_alarms but called from within
    /// interrupt_delay each cycle. Default delegates to process_alarms.
    #[inline]
    fn interrupt_delay_alarms(&mut self, clk: u64) {
        self.process_alarms(clk);
    }

    /// PORT OF: mainc64cpu.c:778 ROM_TRAP_HANDLER() = traps_handler(). Returns 0
    /// if handled in place, a replacement opcode (>0, <0xffffffff) to replay, or
    /// 0xffffffff for a real JAM. Default 0xffffffff (no trap installed → JAM).
    #[inline]
    fn trap_handler(&mut self) -> u32 {
        0xffff_ffff
    }
    /// PORT OF: mainc64cpu.c:805 ROM_TRAP_ALLOWED() = mem_rom_trap_allowed(pc).
    /// Default true (allow the JAM_02 trap check). Implementor narrows to ROM.
    #[inline]
    fn rom_trap_allowed(&mut self, _pc: u16) -> bool {
        true
    }

    /// PORT OF: mainc64cpu.c:780-801 JAM() machine_jam. Returns JAM_NONE /
    /// JAM_RESET_CPU / JAM_POWER_CYCLE / JAM_MONITOR. None ⇒ "no host jam
    /// handler" → caller does the `default: CLK_INC()` path (dtv via JAM).
    #[inline]
    fn jam(&mut self) -> Option<i32> {
        None
    }

    /// PORT OF: mainc64cpu.c:631-651 cpu_reset. Called on IK_RESET dispatch.
    #[inline]
    fn cpu_reset(&mut self) {}

    /// Tracing hook (debug_maincpu, dtv:1828). Default no-op.
    #[inline]
    fn debug_maincpu(&mut self, _pc: u16, _clk: u64, _op: u8, _p1: u8, _p2hi: u8) {}
}

// =============================================================================
// SECTION E — register / execution state.
//
// Mirrors the maincpu locals (mainc64cpu.c:736-746): reg_a/x/y/p/sp + flag_n/
// flag_z split shadows (dtv:68/115-116) + reg_pc (m64:716, an unsigned int) +
// the bank fast-path cache (bank_base/start/limit, m64:719-722) + the jam flag
// (maincpu_jammed, m64:515) + LAST_OPCODE_INFO / LAST_OPCODE_ADDR (m64:483/486).
// =============================================================================

/// C64 6510 register + execution state.
#[derive(Clone, Debug)]
pub struct C64Core6510 {
    pub reg_a: u8,
    pub reg_x: u8,
    pub reg_y: u8,
    pub reg_sp: u8,
    /// P with P_ZERO + P_SIGN masked OUT — flag_n/flag_z are authoritative.
    /// dtv:68 / 117.
    pub reg_p: u8,
    pub reg_pc: u16,
    /// 0x80 if N set, else 0 (VICE flag_n cache). dtv:115.
    pub flag_n: u8,
    /// 0 iff Z set; non-zero iff Z clear (VICE flag_z cache). dtv:116.
    pub flag_z: u8,

    /// Master clock = maincpu_clk (c64cpusc.c:38).
    pub clk: u64,

    /// Bank fast-path cache (m64:719-722). When pinned, `bank_base` is a flat
    /// 64 KB window for fetches with reg_pc in [bank_start, bank_limit). None ⇒
    /// per-byte LOAD fetch (the common SC path — we leave it None unless the
    /// host pins a flat window).
    pub bank_base: Option<Box<[u8; 0x10000]>>,
    pub bank_start: i32,
    pub bank_limit: i32,

    /// last_opcode_info — OPINFO word (m64:483). Aliased into IntStatus.last_opcode_info.
    pub last_opcode_info: u32,
    /// last_opcode_addr (m64:486).
    pub last_opcode_addr: u16,
    /// maincpu_jammed (m64:515).
    pub is_jammed: bool,
}

impl Default for C64Core6510 {
    fn default() -> Self {
        Self::new()
    }
}

impl C64Core6510 {
    /// Power-on state. flags = P_UNUSED; clk = 6 (RESET cycles, m64:643). The
    /// host JUMPs to the reset vector via do_interrupt(IK_RESET) on first run.
    pub fn new() -> Self {
        C64Core6510 {
            reg_a: 0,
            reg_x: 0,
            reg_y: 0,
            reg_sp: 0,
            reg_p: P_UNUSED & !(P_ZERO | P_SIGN),
            reg_pc: 0,
            flag_n: 0,
            flag_z: 1, // Z clear at power-on.
            clk: 6,
            bank_base: None,
            bank_start: 0,
            bank_limit: 0,
            last_opcode_info: 0,
            last_opcode_addr: 0,
            is_jammed: false,
        }
    }

    /// Reset to a known PC with C64 power-on registers, for the full-machine boot
    /// path where the host (Machine::cold_reset) reads the reset vector directly
    /// and seeds the CPU — the reset-vector read is NOT traced and no IK_RESET
    /// dispatch runs (matching the old Cpu6510::reset_to the boot golden was
    /// recorded against). a=x=y=0, sp=$FF, P=$20 (UNUSED only; I is set later by
    /// the KERNAL's own SEI at $FCE4 — the boot trace[0] records P=$20), clk=0.
    pub fn reset_to(&mut self, pc: u16) {
        self.reg_a = 0;
        self.reg_x = 0;
        self.reg_y = 0;
        self.reg_sp = 0xff;
        self.reg_p = P_UNUSED & !(P_ZERO | P_SIGN);
        self.flag_n = 0;
        self.flag_z = 1; // Z clear at power-on.
        self.reg_pc = pc;
        self.clk = 0;
        self.bank_base = None;
        self.bank_start = 0;
        self.bank_limit = 0;
        self.last_opcode_info = 0;
        self.last_opcode_addr = 0;
        self.is_jammed = false;
    }

    /// Composite P (incl. flag_n/flag_z view). = LOCAL_STATUS() (dtv:128-129).
    #[inline]
    pub fn status(&self) -> u8 {
        self.reg_p | (self.flag_n & P_SIGN) | P_UNUSED | (if self.flag_z == 0 { P_ZERO } else { 0 })
    }

    /// Split a composite P back into reg_p + flag_n/flag_z. = LOCAL_SET_STATUS
    /// (dtv:117-119) but without the side effect ordering — for snapshots only.
    #[inline]
    pub fn set_status_composite(&mut self, v: u8) {
        self.reg_p = v & !(P_ZERO | P_SIGN);
        self.flag_n = if v & P_SIGN != 0 { 0x80 } else { 0 };
        self.flag_z = if v & P_ZERO != 0 { 0 } else { 1 };
    }
}

// =============================================================================
// SECTION F — fetch_tab (dtv:1689-1707) — 1 when opcode is 3 bytes.
// =============================================================================
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
// SECTION F2 — OPERAND_BYTES[opcode] = number of operand bytes (0/1/2).
//
// Used by the host trace wrapper to zero `b1`/`b2` for shorter opcodes (the
// CPU_STEP record carries b1=operand-lo, b2=operand-hi, both 0 when the opcode is
// implied/accumulator). This is the canonical NMOS 6502 length table (illegals
// per VICE's decode): implied/accumulator/JAM = 0; immediate/zp/zp,X/zp,Y/
// (zp,X)/(zp),Y/relative = 1; absolute/abs,X/abs,Y/indirect = 2 (= FETCH_TAB).
// =============================================================================
#[rustfmt::skip]
pub const OPERAND_BYTES: [u8; 256] = [
    /*       x0 x1 x2 x3 x4 x5 x6 x7 x8 x9 xA xB xC xD xE xF */
    /* 0x */  0, 1, 0, 1, 1, 1, 1, 1, 0, 1, 0, 1, 2, 2, 2, 2,
    /* 1x */  1, 1, 0, 1, 1, 1, 1, 1, 0, 2, 0, 2, 2, 2, 2, 2,
    /* 2x */  2, 1, 0, 1, 1, 1, 1, 1, 0, 1, 0, 1, 2, 2, 2, 2,
    /* 3x */  1, 1, 0, 1, 1, 1, 1, 1, 0, 2, 0, 2, 2, 2, 2, 2,
    /* 4x */  0, 1, 0, 1, 1, 1, 1, 1, 0, 1, 0, 1, 2, 2, 2, 2,
    /* 5x */  1, 1, 0, 1, 1, 1, 1, 1, 0, 2, 0, 2, 2, 2, 2, 2,
    /* 6x */  0, 1, 0, 1, 1, 1, 1, 1, 0, 1, 0, 1, 2, 2, 2, 2,
    /* 7x */  1, 1, 0, 1, 1, 1, 1, 1, 0, 2, 0, 2, 2, 2, 2, 2,
    /* 8x */  1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 0, 1, 2, 2, 2, 2,
    /* 9x */  1, 1, 0, 1, 1, 1, 1, 1, 0, 2, 0, 2, 2, 2, 2, 2,
    /* Ax */  1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 0, 1, 2, 2, 2, 2,
    /* Bx */  1, 1, 0, 1, 1, 1, 1, 1, 0, 2, 0, 2, 2, 2, 2, 2,
    /* Cx */  1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 0, 1, 2, 2, 2, 2,
    /* Dx */  1, 1, 0, 1, 1, 1, 1, 1, 0, 2, 0, 2, 2, 2, 2, 2,
    /* Ex */  1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 0, 1, 2, 2, 2, 2,
    /* Fx */  1, 1, 0, 1, 1, 1, 1, 1, 0, 2, 0, 2, 2, 2, 2, 2,
];

// =============================================================================
// SECTION G — execution context.
//
// The C64 SC body is one big function over reg_* locals + maincpu_clk + the
// int status. In Rust we bundle them into a per-call Exec borrowing the core,
// the bus, and the int status. Every VICE macro (CLK_INC, LOAD/STORE, the
// GET_*/SET_* addressing helpers, the opcode-body macros) becomes a method on
// Exec. Method names match the VICE macro names (snake/upper) so grep maps 1:1.
// =============================================================================

struct Exec<'a, B: C64Core6510Bus> {
    core: &'a mut C64Core6510,
    bus: &'a mut B,
    int: &'a mut IntStatus,
    /// Result of any JAM dispatch this step.
    jam_result: i32,
    /// True after a JAM_02 trap replaced the opcode → skip the fetch (goto trap_skipped).
    trap_skipped: bool,
    /// The opcode replaced by a trap (set by jam_02).
    trap_opcode: u32,
    /// The (zp),Y RMW shared target address — bridges call_get(IndYRmw) ->
    /// call_set(IndRmw) within one opcode body (mirrors the C `addr` local that
    /// both INT_IND_Y_W and SET_IND_RMW reference). dtv:651-698.
    pending_ind_addr: u16,
}

impl<'a, B: C64Core6510Bus> Exec<'a, B> {
    // -------------------------------------------------------------------------
    // CLK_INC + check_ba (c64cpusc.c:47-51 + mainc64cpu.c:194-208 + :97-110).
    // -------------------------------------------------------------------------

    /// PORT OF: c64cpusc.c:47-51 CLK_INC().
    ///   interrupt_delay(); maincpu_clk++; ba_low &= ~VICII; ba_low |= vicii_cycle();
    /// interrupt_delay (m64:97-110): dispatch alarms up to clk, then bump
    /// irq_delay_cycles / nmi_delay_cycles when the matching *_clk <= clk.
    #[inline]
    fn clk_inc(&mut self) {
        // interrupt_delay() — m64:97-110.
        let clk = self.core.clk;
        self.bus.interrupt_delay_alarms(clk);
        if self.int.irq_clk <= self.core.clk {
            self.int.irq_delay_cycles += 1;
        }
        if self.int.nmi_clk <= self.core.clk {
            self.int.nmi_delay_cycles += 1;
        }
        // maincpu_clk++
        self.core.clk = self.core.clk.wrapping_add(1);
        // ba_low &= ~VICII; ba_low |= vicii_cycle() — the per-cycle VIC tick.
        let c = self.core.clk;
        self.bus.vic_cycle(c);
    }

    /// PORT OF: mainc64cpu.c:194-208 check_ba — steal VIC cycles if BA low. The
    /// implementor advances clk + ticks the VIC for each stolen cycle and
    /// applies the SH*/CLI ENABLES_IRQ steal-signal; it returns the count so we
    /// keep clk in sync (the implementor mutated the shared clk via the trait,
    /// but we mirror it here for safety). `check_ba_low` carries the
    /// LOAD_CHECK_BA_LOW context for SH*.
    #[inline]
    fn check_ba(&mut self) {
        let mut loi = self.core.last_opcode_info;
        let stolen = self.bus.check_ba(&mut loi, false);
        self.core.last_opcode_info = loi;
        self.int.last_opcode_info = loi;
        self.core.clk = self.core.clk.wrapping_add(stolen);
    }
    /// check_ba with the LOAD_CHECK_BA_LOW signal (mainc64cpu.c:400-412) — used
    /// by the SH* stores so maincpu_steal_cycles can set ENABLES_IRQ on a steal.
    #[inline]
    fn check_ba_low(&mut self) {
        let mut loi = self.core.last_opcode_info;
        let stolen = self.bus.check_ba(&mut loi, true);
        self.core.last_opcode_info = loi;
        self.int.last_opcode_info = loi;
        self.core.clk = self.core.clk.wrapping_add(stolen);
    }

    // -------------------------------------------------------------------------
    // LOAD / STORE families (mainc64cpu.c:359-446). Each runs check_ba() first.
    // -------------------------------------------------------------------------
    #[inline]
    fn load(&mut self, a: u16) -> u8 {
        self.check_ba();
        self.bus.read_raw(a)
    }
    /// FETCH load — the opcode/operand byte fetch (FETCH_OPCODE path). Same
    /// check_ba + bus access as `load`, but routes through `read_raw_fetch` so a
    /// tracing implementor tags it as a FETCH (not emitted as a data-bus record).
    #[inline]
    fn load_fetch(&mut self, a: u16) -> u8 {
        self.check_ba();
        self.bus.read_raw_fetch(a)
    }
    #[inline]
    fn load_dummy(&mut self, a: u16) -> u8 {
        self.check_ba();
        self.bus.read_raw_dummy(a)
    }
    /// LOAD_CHECK_BA_LOW (m64:400-405): check_ba_low=1; read; check_ba_low=0.
    #[inline]
    fn load_check_ba_low(&mut self, a: u16) -> u8 {
        self.check_ba_low();
        self.bus.read_raw(a)
    }
    /// LOAD_CHECK_BA_LOW_DUMMY (m64:407-412).
    #[inline]
    fn load_check_ba_low_dummy(&mut self, a: u16) -> u8 {
        self.check_ba_low();
        self.bus.read_raw_dummy(a)
    }
    #[inline]
    fn store(&mut self, a: u16, v: u8) {
        // STORE (m64:372-379): no check_ba on writes (write tab direct).
        self.bus.write_raw(a, v);
    }
    #[inline]
    fn store_dummy(&mut self, a: u16, v: u8) {
        self.bus.write_raw_dummy(a, v);
    }
    #[inline]
    fn load_zero(&mut self, a: u16) -> u8 {
        self.check_ba();
        self.bus.read_raw((a & 0xff) as u16)
    }
    #[inline]
    fn load_zero_dummy(&mut self, a: u16) -> u8 {
        self.check_ba();
        self.bus.read_raw_dummy((a & 0xff) as u16)
    }
    #[inline]
    fn store_zero(&mut self, a: u16, v: u8) {
        self.bus.write_raw((a & 0xff) as u16, v);
    }
    #[inline]
    fn store_zero_dummy(&mut self, a: u16, v: u8) {
        self.bus.write_raw_dummy((a & 0xff) as u16, v);
    }

    // Stack ops (mainc64cpu.c:437-445). PUSH/PULL/STACK_PEEK.
    #[inline]
    fn push(&mut self, v: u8) {
        self.bus.write_raw(0x100u16.wrapping_add(self.core.reg_sp as u16), v);
        self.core.reg_sp = self.core.reg_sp.wrapping_sub(1);
    }
    #[inline]
    fn pull(&mut self) -> u8 {
        self.core.reg_sp = self.core.reg_sp.wrapping_add(1);
        self.check_ba();
        // Stack PULL is a real read whose VALUE is used, but it is NOT emitted as a
        // data-bus record in the trace contract (cpu.rs popped the stack via a bare
        // `load` with no on_bus emit). The dummy variant does the real read for the
        // value and tags it DummyRead (filtered out of the trace). The stack is
        // always RAM ($0100-$01FF), so dummy vs real has no side-effect difference.
        self.bus.read_raw_dummy(0x100u16.wrapping_add(self.core.reg_sp as u16))
    }
    #[inline]
    fn stack_peek(&mut self) -> u8 {
        self.check_ba();
        self.bus.read_raw_dummy(0x100u16.wrapping_add(self.core.reg_sp as u16))
    }

    // -------------------------------------------------------------------------
    // JUMP (mainc64cpu.c:85-91). Updates the cached bank fast-path window.
    // -------------------------------------------------------------------------
    #[inline]
    fn jump(&mut self, addr: u16) {
        self.core.reg_pc = addr;
        if (self.core.reg_pc as i32) >= self.core.bank_limit
            || (self.core.reg_pc as i32) < self.core.bank_start
        {
            // mem_mmu_translate — our bus exposes the flat window only when the
            // host pinned bank_base; without a translate accessor an out-of-window
            // JUMP clears the cache so subsequent fetches take the per-byte path.
            if self.core.bank_base.is_none() {
                self.core.bank_start = 0;
                self.core.bank_limit = 0;
            }
        }
    }

    // -------------------------------------------------------------------------
    // Flag helpers (dtv:68-129).
    // -------------------------------------------------------------------------
    #[inline]
    fn local_set_nz(&mut self, val: u8) {
        self.core.flag_z = val;
        self.core.flag_n = val;
    }
    #[inline]
    fn local_set_overflow(&mut self, val: bool) {
        if val {
            self.core.reg_p |= P_OVERFLOW;
        } else {
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
    // Last-opcode-info bookkeeping (dtv:131-165). Aliases IntStatus.last_opcode_info.
    // -------------------------------------------------------------------------
    #[inline]
    fn set_last_opcode(&mut self, x: u32) {
        // OPINFO_SET(LAST_OPCODE_INFO, x, 0, 0, 0) clears delays/disables/enables.
        self.core.last_opcode_info = x & 0xff;
        self.int.last_opcode_info = self.core.last_opcode_info;
    }
    #[inline]
    fn opcode_delays_interrupt(&mut self) {
        self.core.last_opcode_info |= OPINFO_DELAYS_INTERRUPT_MSK;
        self.int.last_opcode_info = self.core.last_opcode_info;
    }
    #[inline]
    fn opcode_disables_irq(&mut self) {
        self.core.last_opcode_info |= OPINFO_DISABLES_IRQ_MSK;
        self.int.last_opcode_info = self.core.last_opcode_info;
    }
    #[inline]
    fn opcode_enables_irq(&mut self) {
        self.core.last_opcode_info |= OPINFO_ENABLES_IRQ_MSK;
        self.int.last_opcode_info = self.core.last_opcode_info;
    }
    /// LAST_OPCODE_INFO &= ~OPINFO_ENABLES_IRQ_MSK (dtv:708/869/1066/1324) —
    /// the SH*/ANE/LXA/CLI "remove the steal signal".
    #[inline]
    fn opcode_clear_enables_irq(&mut self) {
        self.core.last_opcode_info &= !OPINFO_ENABLES_IRQ_MSK;
        self.int.last_opcode_info = self.core.last_opcode_info;
    }
    #[inline]
    fn opinfo_enables_irq_set(&self) -> bool {
        opinfo_enables_irq(self.core.last_opcode_info) != 0
    }
    #[inline]
    fn set_last_addr(&mut self, x: u16) {
        self.core.last_opcode_addr = x;
    }

    #[inline]
    fn process_alarms(&mut self) {
        let clk = self.core.clk;
        self.bus.process_alarms(clk);
    }

    #[inline]
    fn inc_pc(&mut self, value: u16) {
        self.core.reg_pc = self.core.reg_pc.wrapping_add(value);
    }

    // =========================================================================
    // SECTION H — addressing-mode GET_*/SET_* helpers (dtv:464-720).
    //
    // These are the cycle-stepped addressing modes: each thread CLK_INC()
    // between the dummy read and the real access, and embed the page-cross
    // dummy reads (INT_ABS_I_R / INT_ABS_I_W / INT_ZERO_I / INT_IND_*). Unlike
    // 6510core.c (whole-instruction), every access here is its own cycle.
    // p1/p2 are passed in (computed at fetch); reg_x/reg_y are read live.
    // =========================================================================

    // GET_IMM (dtv:466): dest = p1.  (no cycle — operand already fetched.)
    #[inline]
    fn get_imm(&self, p1: u8) -> u8 {
        p1
    }
    // GET_IMM_DUMMY (dtv:468): nothing.
    #[inline]
    fn get_imm_dummy(&self) {}

    // GET_ABS (dtv:470-472): dest = LOAD(p2); CLK_INC().
    #[inline]
    fn get_abs(&mut self, p2: u16) -> u8 {
        let v = self.load(p2);
        self.clk_inc();
        v
    }
    // GET_ABS_DUMMY (dtv:475-477).
    #[inline]
    fn get_abs_dummy(&mut self, p2: u16) {
        self.load_dummy(p2);
        self.clk_inc();
    }
    // SET_ABS (dtv:479-481): STORE(p2, value); CLK_INC().
    #[inline]
    fn set_abs(&mut self, p2: u16, value: u8) {
        self.store(p2, value);
        self.clk_inc();
    }
    // SET_ABS_RMW (dtv:483-489): STORE_DUMMY(p2, old); CLK_INC(); STORE(p2, new); CLK_INC().
    #[inline]
    fn set_abs_rmw(&mut self, p2: u16, old_value: u8, new_value: u8) {
        // !SKIP_CYCLE is always true (SKIP_CYCLE==0).
        self.store_dummy(p2, old_value);
        self.clk_inc();
        self.store(p2, new_value);
        self.clk_inc();
    }

    // INT_ABS_I_R(reg_i) (dtv:491-495): page-cross dummy read on a read op.
    #[inline]
    fn int_abs_i_r(&mut self, p2: u16, reg_i: u8) {
        if ((p2 & 0xff) as u32 + reg_i as u32) > 0xff {
            self.load_dummy(((p2.wrapping_add(reg_i as u16)) & 0xff) | (p2 & 0xff00));
            self.clk_inc();
        }
    }
    // INT_ABS_I_W(reg_i) (dtv:497-501): always-present dummy read on a write op.
    #[inline]
    fn int_abs_i_w(&mut self, p2: u16, reg_i: u8) {
        self.load_dummy(((p2.wrapping_add(reg_i as u16)) & 0xff) | (p2 & 0xff00));
        self.clk_inc();
    }

    // GET_ABS_X (dtv:503-506).
    #[inline]
    fn get_abs_x(&mut self, p2: u16) -> u8 {
        self.int_abs_i_r(p2, self.core.reg_x);
        let v = self.load(p2.wrapping_add(self.core.reg_x as u16));
        self.clk_inc();
        v
    }
    // GET_ABS_X_DUMMY (dtv:508-511).
    #[inline]
    fn get_abs_x_dummy(&mut self, p2: u16) {
        self.int_abs_i_r(p2, self.core.reg_x);
        self.load_dummy(p2.wrapping_add(self.core.reg_x as u16));
        self.clk_inc();
    }
    // GET_ABS_Y (dtv:513-516).
    #[inline]
    fn get_abs_y(&mut self, p2: u16) -> u8 {
        self.int_abs_i_r(p2, self.core.reg_y);
        let v = self.load(p2.wrapping_add(self.core.reg_y as u16));
        self.clk_inc();
        v
    }
    // GET_ABS_X_RMW (dtv:518-521).
    #[inline]
    fn get_abs_x_rmw(&mut self, p2: u16) -> u8 {
        self.int_abs_i_w(p2, self.core.reg_x);
        let v = self.load(p2.wrapping_add(self.core.reg_x as u16));
        self.clk_inc();
        v
    }
    // GET_ABS_Y_RMW (dtv:523-526).
    #[inline]
    fn get_abs_y_rmw(&mut self, p2: u16) -> u8 {
        self.int_abs_i_w(p2, self.core.reg_y);
        let v = self.load(p2.wrapping_add(self.core.reg_y as u16));
        self.clk_inc();
        v
    }
    // SET_ABS_X (dtv:528-531).
    #[inline]
    fn set_abs_x(&mut self, p2: u16, value: u8) {
        self.int_abs_i_w(p2, self.core.reg_x);
        self.store(p2.wrapping_add(self.core.reg_x as u16), value);
        self.clk_inc();
    }
    // SET_ABS_Y (dtv:533-536).
    #[inline]
    fn set_abs_y(&mut self, p2: u16, value: u8) {
        self.int_abs_i_w(p2, self.core.reg_y);
        self.store(p2.wrapping_add(self.core.reg_y as u16), value);
        self.clk_inc();
    }
    // SET_ABS_I_RMW (dtv:538-544).
    #[inline]
    fn set_abs_i_rmw(&mut self, p2: u16, reg_i: u8, old_value: u8, new_value: u8) {
        self.store_dummy(p2.wrapping_add(reg_i as u16), old_value);
        self.clk_inc();
        self.store(p2.wrapping_add(reg_i as u16), new_value);
        self.clk_inc();
    }
    #[inline]
    fn set_abs_x_rmw(&mut self, p2: u16, old_value: u8, new_value: u8) {
        self.set_abs_i_rmw(p2, self.core.reg_x, old_value, new_value);
    }
    #[inline]
    fn set_abs_y_rmw(&mut self, p2: u16, old_value: u8, new_value: u8) {
        self.set_abs_i_rmw(p2, self.core.reg_y, old_value, new_value);
    }

    // GET_ZERO (dtv:550-552).
    #[inline]
    fn get_zero(&mut self, p1: u8) -> u8 {
        let v = self.load_zero(p1 as u16);
        self.clk_inc();
        v
    }
    // GET_ZERO_DUMMY (dtv:555-557).
    #[inline]
    fn get_zero_dummy(&mut self, p1: u8) {
        self.load_zero_dummy(p1 as u16);
        self.clk_inc();
    }
    // SET_ZERO (dtv:559-561).
    #[inline]
    fn set_zero(&mut self, p1: u8, value: u8) {
        self.store_zero(p1 as u16, value);
        self.clk_inc();
    }
    // SET_ZERO_RMW (dtv:563-569).
    #[inline]
    fn set_zero_rmw(&mut self, p1: u8, old_value: u8, new_value: u8) {
        self.store_zero_dummy(p1 as u16, old_value);
        self.clk_inc();
        self.store_zero(p1 as u16, new_value);
        self.clk_inc();
    }
    // INT_ZERO_I (dtv:571-575).
    #[inline]
    fn int_zero_i(&mut self, p1: u8) {
        self.load_zero_dummy(p1 as u16);
        self.clk_inc();
    }
    // GET_ZERO_X (dtv:578-581).
    #[inline]
    fn get_zero_x(&mut self, p1: u8) -> u8 {
        self.int_zero_i(p1);
        let v = self.load_zero(p1.wrapping_add(self.core.reg_x) as u16);
        self.clk_inc();
        v
    }
    // GET_ZERO_X_DUMMY (dtv:583-586).
    #[inline]
    fn get_zero_x_dummy(&mut self, p1: u8) {
        self.int_zero_i(p1);
        self.load_zero_dummy(p1.wrapping_add(self.core.reg_x) as u16);
        self.clk_inc();
    }
    // GET_ZERO_Y (dtv:588-591).
    #[inline]
    fn get_zero_y(&mut self, p1: u8) -> u8 {
        self.int_zero_i(p1);
        let v = self.load_zero(p1.wrapping_add(self.core.reg_y) as u16);
        self.clk_inc();
        v
    }
    // SET_ZERO_X (dtv:593-596).
    #[inline]
    fn set_zero_x(&mut self, p1: u8, value: u8) {
        self.int_zero_i(p1);
        self.store_zero(p1.wrapping_add(self.core.reg_x) as u16, value);
        self.clk_inc();
    }
    // SET_ZERO_Y (dtv:598-601).
    #[inline]
    fn set_zero_y(&mut self, p1: u8, value: u8) {
        self.int_zero_i(p1);
        self.store_zero(p1.wrapping_add(self.core.reg_y) as u16, value);
        self.clk_inc();
    }
    // SET_ZERO_I_RMW (dtv:603-609).
    #[inline]
    fn set_zero_i_rmw(&mut self, p1: u8, reg_i: u8, old_value: u8, new_value: u8) {
        self.store_zero_dummy(p1.wrapping_add(reg_i) as u16, old_value);
        self.clk_inc();
        self.store_zero(p1.wrapping_add(reg_i) as u16, new_value);
        self.clk_inc();
    }
    #[inline]
    fn set_zero_x_rmw(&mut self, p1: u8, old_value: u8, new_value: u8) {
        self.set_zero_i_rmw(p1, self.core.reg_x, old_value, new_value);
    }
    #[inline]
    fn set_zero_y_rmw(&mut self, p1: u8, old_value: u8, new_value: u8) {
        self.set_zero_i_rmw(p1, self.core.reg_y, old_value, new_value);
    }

    // INT_IND_X (dtv:615-624): returns the final target address `addr`. 3 cycles.
    #[inline]
    fn int_ind_x(&mut self, p1: u8) -> u16 {
        self.load_zero_dummy(p1 as u16);
        self.clk_inc();
        let mut tmpa = p1.wrapping_add(self.core.reg_x) as u16;
        let mut addr = self.load_zero(tmpa) as u16;
        self.clk_inc();
        tmpa = (tmpa.wrapping_add(1)) & 0xff;
        addr |= (self.load_zero(tmpa) as u16) << 8;
        self.clk_inc();
        addr
    }
    // GET_IND_X (dtv:627-630).
    #[inline]
    fn get_ind_x(&mut self, p1: u8) -> u8 {
        let addr = self.int_ind_x(p1);
        let v = self.load(addr);
        self.clk_inc();
        v
    }
    // SET_IND_X (dtv:632-637).
    #[inline]
    fn set_ind_x(&mut self, p1: u8, value: u8) {
        let addr = self.int_ind_x(p1);
        self.store(addr, value);
        self.clk_inc();
    }

    // INT_IND_Y_R (dtv:639-649): returns the final addr. The read variant only
    // emits the page-cross dummy read.
    #[inline]
    fn int_ind_y_r(&mut self, p1: u8) -> u16 {
        let mut tmpa = self.load_zero(p1 as u16) as u16;
        self.clk_inc();
        tmpa |= (self.load_zero(p1.wrapping_add(1) as u16) as u16) << 8;
        self.clk_inc();
        if ((tmpa & 0xff) as u32 + self.core.reg_y as u32) > 0xff {
            self.load_dummy((tmpa & 0xff00) | ((tmpa.wrapping_add(self.core.reg_y as u16)) & 0xff));
            self.clk_inc();
        }
        tmpa.wrapping_add(self.core.reg_y as u16) & 0xffff
    }
    // INT_IND_Y_W (dtv:651-661): the write variant always emits the dummy read.
    #[inline]
    fn int_ind_y_w(&mut self, p1: u8) -> u16 {
        let mut tmpa = self.load_zero(p1 as u16) as u16;
        self.clk_inc();
        tmpa |= (self.load_zero(p1.wrapping_add(1) as u16) as u16) << 8;
        self.clk_inc();
        self.load_dummy((tmpa & 0xff00) | ((tmpa.wrapping_add(self.core.reg_y as u16)) & 0xff));
        self.clk_inc();
        tmpa.wrapping_add(self.core.reg_y as u16) & 0xffff
    }
    // INT_IND_Y_W_NOADDR (dtv:663-672): for SHA_IND_Y — returns the base tmpa
    // (NOT +Y), emits LOAD_CHECK_BA_LOW for the page-cross dummy so the SH* steal
    // signal can fire.
    #[inline]
    fn int_ind_y_w_noaddr(&mut self, p1: u8) -> u16 {
        let mut tmpa = self.load_zero(p1 as u16) as u16;
        self.clk_inc();
        tmpa |= (self.load_zero(p1.wrapping_add(1) as u16) as u16) << 8;
        self.clk_inc();
        self.load_check_ba_low(
            (tmpa & 0xff00) | ((tmpa.wrapping_add(self.core.reg_y as u16)) & 0xff),
        );
        self.clk_inc();
        tmpa
    }
    // GET_IND_Y (dtv:675-678).
    #[inline]
    fn get_ind_y(&mut self, p1: u8) -> u8 {
        let addr = self.int_ind_y_r(p1);
        let v = self.load(addr);
        self.clk_inc();
        v
    }
    // GET_IND_Y_RMW (dtv:680-683).
    #[inline]
    fn get_ind_y_rmw(&mut self, p1: u8) -> (u16, u8) {
        let addr = self.int_ind_y_w(p1);
        let v = self.load(addr);
        self.clk_inc();
        (addr, v)
    }
    // SET_IND_Y (dtv:685-690).
    #[inline]
    fn set_ind_y(&mut self, p1: u8, value: u8) {
        let addr = self.int_ind_y_w(p1);
        self.store(addr, value);
        self.clk_inc();
    }
    // SET_IND_RMW (dtv:692-698): the RMW write-back for an (zp),Y / (zp,X) RMW.
    #[inline]
    fn set_ind_rmw(&mut self, addr: u16, old_value: u8, new_value: u8) {
        self.store_dummy(addr, old_value);
        self.clk_inc();
        self.store(addr, new_value);
        self.clk_inc();
    }

    // SET_ABS_SH_I(addr, reg_and, reg_i) (dtv:700-720) — THE SH* store with the
    // page-cross target-high-byte corruption + the steal-signal ENABLES_IRQ
    // bypass. tmp3 = reg_and & ((addr>>8)+1) is the stored value; on a page
    // cross the target high byte is REPLACED by tmp3 (the corruption). If a
    // steal set ENABLES_IRQ after the BA-low dummy, the value is NOT ANDed.
    #[inline]
    fn set_abs_sh_i(&mut self, addr: u16, reg_and: u8, reg_i: u8) {
        let tmp3 = (reg_and & (((addr >> 8) as u8).wrapping_add(1))) as u16;
        let value: u16;
        if self.opinfo_enables_irq_set() {
            self.opcode_clear_enables_irq();
            value = reg_and as u16; // not ANDed
        } else {
            value = tmp3;
        }
        let mut tmp2 = addr.wrapping_add(reg_i as u16);
        if ((addr & 0xff) as u32 + reg_i as u32) > 0xff {
            tmp2 = (tmp2 & 0xff) | (tmp3 << 8);
        }
        self.store(tmp2, value as u8);
        self.clk_inc();
    }

    // =========================================================================
    // SECTION I — opcode-body helpers (dtv:739-1685). Each matches a VICE macro.
    //
    // The get_func/set_func parametrisation is realised with the GetFn/SetFn
    // dispatch enums (below) so the opcode switch can pass "GET_ZERO" etc. The
    // ALU math is verbatim from the macros (cross-checked against drive_6510core.rs).
    // =========================================================================

    // ADC (dtv:739-771): get_func(tmp_value) then the decimal/binary add.
    fn adc(&mut self, value: u8, pc_inc: u16) {
        let tmp_value = value as u32;
        let tmp: u32;
        if self.local_decimal() {
            let mut t: u32 =
                (self.core.reg_a as u32 & 0xf) + (tmp_value & 0xf) + (self.core.reg_p as u32 & 0x1);
            if t > 0x9 {
                t += 0x6;
            }
            if t <= 0x0f {
                t = (t & 0xf) + (self.core.reg_a as u32 & 0xf0) + (tmp_value & 0xf0);
            } else {
                t = (t & 0xf) + (self.core.reg_a as u32 & 0xf0) + (tmp_value & 0xf0) + 0x10;
            }
            self.local_set_zero(
                ((self.core.reg_a as u32 + tmp_value + (self.core.reg_p as u32 & 0x1)) & 0xff) == 0,
            );
            self.local_set_sign(t & 0x80 != 0);
            self.local_set_overflow(
                ((self.core.reg_a as u32 ^ t) & 0x80) != 0
                    && ((self.core.reg_a as u32 ^ tmp_value) & 0x80) == 0,
            );
            if (t & 0x1f0) > 0x90 {
                t += 0x60;
            }
            self.local_set_carry((t & 0xff0) > 0xf0);
            tmp = t;
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

    // ANC (dtv:773-779).
    fn anc(&mut self, p1: u8) {
        self.core.reg_a &= p1;
        self.local_set_nz(self.core.reg_a);
        self.local_set_carry(self.local_sign());
        self.inc_pc(2);
    }

    // AND (dtv:781-788).
    fn and(&mut self, value: u8, pc_inc: u16) {
        self.core.reg_a &= value;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(pc_inc);
    }

    // ANE (dtv:864-882): the steal-signal RDY variant + the "pretend NOP #$nn"
    // SET_LAST_OPCODE(0x80) at the end.
    fn ane(&mut self, p1: u8) {
        if self.opinfo_enables_irq_set() {
            self.opcode_clear_enables_irq();
            self.core.reg_a = (self.core.reg_a | ANE_RDY_MAGIC) & self.core.reg_x & p1;
        } else {
            self.core.reg_a = (self.core.reg_a | ANE_MAGIC) & self.core.reg_x & p1;
        }
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(2);
        self.set_last_opcode(0x80);
    }

    // ARR (dtv:885-916).
    fn arr(&mut self, p1: u8) {
        let tmp = (self.core.reg_a & p1) as u32;
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
        self.inc_pc(2);
    }

    // ASL_A (dtv:929-935).
    fn asl_a(&mut self) {
        self.local_set_carry(self.core.reg_a & 0x80 != 0);
        self.core.reg_a = (self.core.reg_a as u16 as u32).wrapping_shl(1) as u8;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(1);
    }
    // ASR (dtv:937-946).
    fn asr(&mut self, p1: u8) {
        let tmp = self.core.reg_a & p1;
        self.local_set_carry(tmp & 0x01 != 0);
        self.core.reg_a = tmp >> 1;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(2);
    }

    // CLC/CLD/CLV (dtv:1042-1052/1084-1088).
    fn clc(&mut self) {
        self.inc_pc(1);
        self.local_set_carry(false);
    }
    fn cld(&mut self) {
        self.inc_pc(1);
        self.local_set_decimal(false);
    }
    fn clv(&mut self) {
        self.inc_pc(1);
        self.local_set_overflow(false);
    }
    // CLI (dtv:1056-1069, OPCODE_UPDATE_IN_FETCH variant): the steal-signal
    // "shouldn't delay" path.
    fn cli(&mut self) {
        self.inc_pc(1);
        if !self.opinfo_enables_irq_set() {
            if self.local_interrupt() {
                self.opcode_enables_irq();
            }
        } else {
            self.opcode_clear_enables_irq();
        }
        self.local_set_interrupt(false);
    }

    // CP(reg, value) (dtv:1090-1099): generic compare.
    fn cp(&mut self, reg: u8, value: u8, pc_inc: u16) {
        let tmp = (reg as u32).wrapping_sub(value as u32);
        self.local_set_carry(tmp < 0x100);
        self.local_set_nz((tmp & 0xff) as u8);
        self.inc_pc(pc_inc);
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

    fn eor(&mut self, value: u8, pc_inc: u16) {
        self.core.reg_a ^= value;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(pc_inc);
    }
    fn ora(&mut self, value: u8, pc_inc: u16) {
        self.core.reg_a |= value;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(pc_inc);
    }

    // LSR_A / ROL_A / ROR_A (dtv:1257-1263/1421-1429/1446-1454).
    fn lsr_a(&mut self) {
        self.local_set_carry(self.core.reg_a & 0x01 != 0);
        self.core.reg_a >>= 1;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(1);
    }
    fn rol_a(&mut self) {
        let tmp = (self.core.reg_a as u32) << 1;
        self.core.reg_a = (tmp | (self.core.reg_p as u32 & P_CARRY as u32)) as u8;
        self.local_set_carry(tmp & 0x100 != 0);
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(1);
    }
    fn ror_a(&mut self) {
        let tmp = self.core.reg_a;
        self.core.reg_a = (self.core.reg_a >> 1) | ((self.core.reg_p as u16) << 7) as u8;
        self.local_set_carry(tmp & 0x01 != 0);
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(1);
    }

    // LXA (dtv:1319-1337): the steal-signal RDY variant + pretend-NOP.
    fn lxa(&mut self, p1: u8) {
        if self.opinfo_enables_irq_set() {
            self.opcode_clear_enables_irq();
            self.core.reg_a = (self.core.reg_a | LXA_RDY_MAGIC) & p1;
        } else {
            self.core.reg_a = (self.core.reg_a | LXA_MAGIC) & p1;
        }
        self.core.reg_x = self.core.reg_a;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(2);
        self.set_last_opcode(0x80);
    }

    // SBC (dtv:1521-1549).
    fn sbc(&mut self, value: u8, pc_inc: u16) {
        let src = value as u32;
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
    // SBX (dtv:1551-1559).
    fn sbx(&mut self, p1: u8) {
        self.inc_pc(2);
        let tmp = ((self.core.reg_a & self.core.reg_x) as u32).wrapping_sub(p1 as u32);
        self.local_set_carry(tmp < 0x100);
        self.core.reg_x = (tmp & 0xff) as u8;
        self.local_set_nz(self.core.reg_x);
    }

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

    fn tax(&mut self) {
        self.core.reg_x = self.core.reg_a;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(1);
    }
    fn tay(&mut self) {
        self.core.reg_y = self.core.reg_a;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(1);
    }
    fn tsx(&mut self) {
        self.core.reg_x = self.core.reg_sp;
        self.local_set_nz(self.core.reg_sp);
        self.inc_pc(1);
    }
    fn txa(&mut self) {
        self.core.reg_a = self.core.reg_x;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(1);
    }
    fn txs(&mut self) {
        self.core.reg_sp = self.core.reg_x;
        self.inc_pc(1);
    }
    fn tya(&mut self) {
        self.core.reg_a = self.core.reg_y;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(1);
    }

    // -------------------------------------------------------------------------
    // BRANCH (dtv:986-1005, non-DTV). dummy fetch + page-cross dummy + delay.
    // -------------------------------------------------------------------------
    fn branch(&mut self, cond: bool, p1: u8) {
        self.inc_pc(2);
        if cond {
            let dest_addr = self.core.reg_pc.wrapping_add((p1 as i8) as u16);
            self.load_dummy(self.core.reg_pc);
            self.clk_inc();
            if (self.core.reg_pc ^ dest_addr) & 0xff00 != 0 {
                self.load_dummy((self.core.reg_pc & 0xff00) | (dest_addr & 0xff));
                self.clk_inc();
            } else {
                self.opcode_delays_interrupt();
            }
            self.jump(dest_addr);
        }
    }

    // -------------------------------------------------------------------------
    // JMP / JMP_IND / JSR / RTS / RTI (dtv:1179-1220/1476-1512).
    // -------------------------------------------------------------------------
    fn jmp(&mut self, addr: u16) {
        self.jump(addr);
    }
    fn jmp_ind(&mut self, p2: u16) {
        let mut dest_addr = self.load(p2) as u16;
        self.clk_inc();
        dest_addr |= (self.load((p2 & 0xff00) | (p2.wrapping_add(1) & 0xff)) as u16) << 8;
        self.clk_inc();
        self.jump(dest_addr);
    }
    fn jsr(&mut self, p1: u8) {
        // !SKIP_CYCLE: STACK_PEEK(); CLK_INC().
        self.stack_peek();
        self.clk_inc();
        self.inc_pc(2);
        self.push(((self.core.reg_pc >> 8) & 0xff) as u8);
        self.clk_inc();
        self.push((self.core.reg_pc & 0xff) as u8);
        self.clk_inc();
        // The target high byte is the instruction's 3rd encoding byte (operand-hi):
        // a FETCH, not a data read — JSR interleaves the stack pushes between the
        // lo and hi fetch, so it is read here rather than in FETCH_OPCODE.
        let addr_msb = self.load_fetch(self.core.reg_pc);
        let dest_addr = (p1 as u16) | ((addr_msb as u16) << 8);
        self.clk_inc();
        self.jump(dest_addr);
    }
    fn rts(&mut self) {
        self.stack_peek();
        self.clk_inc();
        let mut tmp = self.pull() as u16;
        self.clk_inc();
        tmp |= (self.pull() as u16) << 8;
        self.clk_inc();
        // The read at the return address is the throwaway cycle (PC is set to
        // tmp+1) — a DUMMY read in the trace contract (not emitted), matching
        // cpu.rs's RTS final cycle.
        self.load_dummy(tmp);
        self.clk_inc();
        tmp = tmp.wrapping_add(1);
        self.jump(tmp);
    }
    fn rti(&mut self) {
        self.stack_peek();
        self.clk_inc();
        let mut tmp = self.pull() as u16;
        self.clk_inc();
        self.local_set_status((tmp & 0xff) as u8);
        tmp = self.pull() as u16;
        self.clk_inc();
        tmp |= (self.pull() as u16) << 8;
        self.clk_inc();
        self.jump(tmp);
    }

    // -------------------------------------------------------------------------
    // PHA / PHP / PLA / PLP (dtv:1354-1396).
    // -------------------------------------------------------------------------
    fn pha(&mut self) {
        self.push(self.core.reg_a);
        self.clk_inc();
        self.inc_pc(1);
    }
    fn php(&mut self) {
        let st = self.local_status() | P_BREAK;
        self.push(st);
        self.clk_inc();
        self.inc_pc(1);
    }
    fn pla(&mut self) {
        self.stack_peek();
        self.clk_inc();
        self.core.reg_a = self.pull();
        self.clk_inc();
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(1);
    }
    fn plp(&mut self) {
        self.stack_peek();
        self.clk_inc();
        let s = self.pull();
        self.clk_inc();
        if (s & P_INTERRUPT) == 0 && self.local_interrupt() {
            self.opcode_enables_irq();
        } else if (s & P_INTERRUPT) != 0 && !self.local_interrupt() {
            self.opcode_disables_irq();
        }
        self.local_set_status(s);
        self.inc_pc(1);
    }

    // -------------------------------------------------------------------------
    // RMW + combined illegal ops dispatched on get_func/set_func (GetFn/SetFn).
    // Each ports the corresponding dtv macro (ASL/LSR/ROL/ROR/DEC/INC/DCP/ISB/
    // RLA/RRA/SLO/SRE/BIT/LD/LAX/CP/ADC/AND/EOR/ORA/SBC/ST). The get_func reads
    // old_value (advancing cycles); set_func writes back (advancing cycles).
    // -------------------------------------------------------------------------

    // ASL (dtv:918-927).
    fn asl(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn, sf: SetFn) {
        let old_value = self.call_get(gf, p1, p2);
        self.local_set_carry(old_value & 0x80 != 0);
        let new_value = ((old_value as u16) << 1) as u8;
        self.local_set_nz(new_value);
        self.inc_pc(pc_inc);
        self.call_set(sf, p1, p2, old_value, new_value);
    }
    // LSR (dtv:1246-1255).
    fn lsr(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn, sf: SetFn) {
        let old_value = self.call_get(gf, p1, p2);
        self.local_set_carry(old_value & 0x01 != 0);
        let new_value = old_value >> 1;
        self.local_set_nz(new_value);
        self.inc_pc(pc_inc);
        self.call_set(sf, p1, p2, old_value, new_value);
    }
    // ROL (dtv:1410-1419).
    fn rol(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn, sf: SetFn) {
        let old_value = self.call_get(gf, p1, p2) as u16;
        let new_value = (old_value << 1) | (self.core.reg_p as u16 & P_CARRY as u16);
        self.local_set_carry(new_value & 0x100 != 0);
        self.local_set_nz((new_value & 0xff) as u8);
        self.inc_pc(pc_inc);
        self.call_set(sf, p1, p2, old_value as u8, (new_value & 0xff) as u8);
    }
    // ROR (dtv:1431-1444).
    fn ror(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn, sf: SetFn) {
        let old_value = self.call_get(gf, p1, p2);
        let mut new_value = old_value as u16;
        if self.core.reg_p & P_CARRY != 0 {
            new_value |= 0x100;
        }
        self.local_set_carry(new_value & 0x01 != 0);
        new_value >>= 1;
        self.local_set_nz((new_value & 0xff) as u8);
        self.inc_pc(pc_inc);
        self.call_set(sf, p1, p2, old_value, (new_value & 0xff) as u8);
    }
    // DEC (dtv:1112-1120).
    fn dec(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn, sf: SetFn) {
        let old_value = self.call_get(gf, p1, p2);
        let new_value = old_value.wrapping_sub(1);
        self.local_set_nz(new_value);
        self.inc_pc(pc_inc);
        self.call_set(sf, p1, p2, old_value, new_value);
    }
    // INC (dtv:1145-1153).
    fn inc(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn, sf: SetFn) {
        let old_value = self.call_get(gf, p1, p2);
        let new_value = old_value.wrapping_add(1);
        self.local_set_nz(new_value);
        self.inc_pc(pc_inc);
        self.call_set(sf, p1, p2, old_value, new_value);
    }
    // DCP (dtv:1101-1110).
    fn dcp(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn, sf: SetFn) {
        let old_value = self.call_get(gf, p1, p2);
        let new_value = old_value.wrapping_sub(1);
        self.local_set_carry(self.core.reg_a >= new_value);
        self.local_set_nz(self.core.reg_a.wrapping_sub(new_value));
        self.inc_pc(pc_inc);
        self.call_set(sf, p1, p2, old_value, new_value);
    }
    // ISB (dtv:1169-1177): get; new=old+1; SBC(new); set.
    fn isb(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn, sf: SetFn) {
        let old_value = self.call_get(gf, p1, p2);
        let new_value = old_value.wrapping_add(1);
        self.sbc(new_value, 0);
        self.inc_pc(pc_inc);
        self.call_set(sf, p1, p2, old_value, new_value);
    }
    // RLA (dtv:1398-1408).
    fn rla(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn, sf: SetFn) {
        let old_value = self.call_get(gf, p1, p2) as u16;
        let new_value = (old_value << 1) | (self.core.reg_p as u16 & P_CARRY as u16);
        self.local_set_carry(new_value & 0x100 != 0);
        self.core.reg_a &= new_value as u8;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(pc_inc);
        self.call_set(sf, p1, p2, old_value as u8, (new_value & 0xff) as u8);
    }
    // RRA (dtv:1456-1470): get; ROR-style; ADC(new); set.
    fn rra(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn, sf: SetFn) {
        let old_value = self.call_get(gf, p1, p2);
        let mut new_value = old_value as u16;
        if self.core.reg_p & P_CARRY != 0 {
            new_value |= 0x100;
        }
        self.local_set_carry(new_value & 0x01 != 0);
        new_value >>= 1;
        self.local_set_nz((new_value & 0xff) as u8);
        self.inc_pc(pc_inc);
        self.adc((new_value & 0xff) as u8, 0);
        self.call_set(sf, p1, p2, old_value, (new_value & 0xff) as u8);
    }
    // SLO (dtv:1613-1624).
    fn slo(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn, sf: SetFn) {
        let old_value = self.call_get(gf, p1, p2);
        self.local_set_carry(old_value & 0x80 != 0);
        let new_value = ((old_value as u16) << 1) as u8;
        self.core.reg_a |= new_value;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(pc_inc);
        self.call_set(sf, p1, p2, old_value, new_value);
    }
    // SRE (dtv:1626-1637).
    fn sre(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn, sf: SetFn) {
        let old_value = self.call_get(gf, p1, p2);
        self.local_set_carry(old_value & 0x01 != 0);
        let new_value = old_value >> 1;
        self.core.reg_a ^= new_value;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(pc_inc);
        self.call_set(sf, p1, p2, old_value, new_value);
    }
    // BIT (dtv:948-956).
    fn bit(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn) {
        let tmp = self.call_get(gf, p1, p2);
        self.local_set_sign(tmp & 0x80 != 0);
        self.local_set_overflow(tmp & 0x40 != 0);
        self.local_set_zero((tmp & self.core.reg_a) == 0);
        self.inc_pc(pc_inc);
    }
    // ADC/AND/EOR/ORA/SBC/CP dispatched on a get_func.
    fn adc_g(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn) {
        let v = self.call_get(gf, p1, p2);
        self.adc(v, pc_inc);
    }
    fn and_g(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn) {
        let v = self.call_get(gf, p1, p2);
        self.and(v, pc_inc);
    }
    fn eor_g(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn) {
        let v = self.call_get(gf, p1, p2);
        self.eor(v, pc_inc);
    }
    fn ora_g(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn) {
        let v = self.call_get(gf, p1, p2);
        self.ora(v, pc_inc);
    }
    fn sbc_g(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn) {
        let v = self.call_get(gf, p1, p2);
        self.sbc(v, pc_inc);
    }
    fn cp_g(&mut self, reg: u8, p1: u8, p2: u16, pc_inc: u16, gf: GetFn) {
        let v = self.call_get(gf, p1, p2);
        self.cp(reg, v, pc_inc);
    }
    // LD (dtv:1239-1244): get; set N/Z; pc. dest selects which reg.
    fn ld(&mut self, dest: LdDest, p1: u8, p2: u16, pc_inc: u16, gf: GetFn) {
        let v = self.call_get(gf, p1, p2);
        match dest {
            LdDest::A => self.core.reg_a = v,
            LdDest::X => self.core.reg_x = v,
            LdDest::Y => self.core.reg_y = v,
        }
        self.local_set_nz(v);
        self.inc_pc(pc_inc);
    }
    // LAX (dtv:1231-1237).
    fn lax(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn) {
        let v = self.call_get(gf, p1, p2);
        self.core.reg_x = v;
        self.core.reg_a = v;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(pc_inc);
    }
    // LAS (dtv:1222-1229): GET_ABS_Y; A=X=SP = SP & value.
    fn las(&mut self, p2: u16) {
        let value = self.get_abs_y(p2);
        self.core.reg_sp &= value;
        self.core.reg_x = self.core.reg_sp;
        self.core.reg_a = self.core.reg_sp;
        self.local_set_nz(self.core.reg_a);
        self.inc_pc(3);
    }
    // NOOP (dtv:1348-1352): get_func() (dummy) then pc.
    fn noop(&mut self, p1: u8, p2: u16, pc_inc: u16, gf: GetFn) {
        self.call_get_dummy(gf, p1, p2);
        self.inc_pc(pc_inc);
    }
    // ST (dtv:1639-1643): pc THEN set_func(value).
    fn st(&mut self, value: u8, p1: u8, p2: u16, pc_inc: u16, sf: SetFn) {
        self.inc_pc(pc_inc);
        self.call_set_store(sf, p1, p2, value);
    }

    // SHA_IND_Y (dtv:1583-1588).
    fn sha_ind_y(&mut self, p1: u8) {
        let tmpa = self.int_ind_y_w_noaddr(p1);
        let reg_and = self.core.reg_a & self.core.reg_x;
        self.set_abs_sh_i(tmpa, reg_and, self.core.reg_y);
        self.inc_pc(2);
    }
    // SH_ABS_I(reg_and, reg_i) (dtv:1590-1598): the BA-low dummy + SET_ABS_SH_I + pc.
    fn sh_abs_i(&mut self, p2: u16, reg_and: u8, reg_i: u8) {
        self.load_check_ba_low_dummy(((p2.wrapping_add(reg_i as u16)) & 0xff) | (p2 & 0xff00));
        self.clk_inc();
        self.set_abs_sh_i(p2, reg_and, reg_i);
        self.inc_pc(3);
    }
    // SHS_ABS_Y (dtv:1600-1604): SH_ABS_I(A&X, Y); SP = A&X.
    fn shs_abs_y(&mut self, p2: u16) {
        let reg_and = self.core.reg_a & self.core.reg_x;
        self.sh_abs_i(p2, reg_and, self.core.reg_y);
        self.core.reg_sp = self.core.reg_a & self.core.reg_x;
    }

    // -------------------------------------------------------------------------
    // get_func/set_func dispatch (the GetFn/SetFn enums).
    // -------------------------------------------------------------------------
    #[inline]
    fn call_get(&mut self, gf: GetFn, p1: u8, p2: u16) -> u8 {
        match gf {
            GetFn::Imm => self.get_imm(p1),
            GetFn::Zero => self.get_zero(p1),
            GetFn::ZeroX => self.get_zero_x(p1),
            GetFn::ZeroY => self.get_zero_y(p1),
            GetFn::Abs => self.get_abs(p2),
            GetFn::AbsX => self.get_abs_x(p2),
            GetFn::AbsY => self.get_abs_y(p2),
            GetFn::AbsXRmw => self.get_abs_x_rmw(p2),
            GetFn::AbsYRmw => self.get_abs_y_rmw(p2),
            GetFn::IndX => self.get_ind_x(p1),
            GetFn::IndY => self.get_ind_y(p1),
            GetFn::IndYRmw => {
                let (addr, v) = self.get_ind_y_rmw(p1);
                self.pending_ind_addr = addr;
                v
            }
        }
    }
    /// The dummy GET for NOOP (GET_*_DUMMY / GET_IMM_DUMMY).
    #[inline]
    fn call_get_dummy(&mut self, gf: GetFn, p1: u8, p2: u16) {
        match gf {
            GetFn::Imm => self.get_imm_dummy(),
            GetFn::Zero => self.get_zero_dummy(p1),
            GetFn::ZeroX => self.get_zero_x_dummy(p1),
            GetFn::Abs => self.get_abs_dummy(p2),
            GetFn::AbsX => self.get_abs_x_dummy(p2),
            // Other modes never appear as NOOP dummies in the switch.
            _ => {}
        }
    }
    /// RMW write-back (set_func(old, new)).
    #[inline]
    fn call_set(&mut self, sf: SetFn, p1: u8, p2: u16, old_value: u8, new_value: u8) {
        match sf {
            SetFn::SZeroRmw => self.set_zero_rmw(p1, old_value, new_value),
            SetFn::SZeroXRmw => self.set_zero_x_rmw(p1, old_value, new_value),
            SetFn::SAbsRmw => self.set_abs_rmw(p2, old_value, new_value),
            SetFn::SAbsXRmw => self.set_abs_x_rmw(p2, old_value, new_value),
            SetFn::SAbsYRmw => self.set_abs_y_rmw(p2, old_value, new_value),
            SetFn::SIndRmw => {
                let addr = self.pending_ind_addr;
                self.set_ind_rmw(addr, old_value, new_value);
            }
            // Plain stores never appear as RMW set_func.
            _ => {}
        }
    }
    /// Plain store set_func (set_func(value)) for ST.
    #[inline]
    fn call_set_store(&mut self, sf: SetFn, p1: u8, p2: u16, value: u8) {
        match sf {
            SetFn::SZero => self.set_zero(p1, value),
            SetFn::SZeroX => self.set_zero_x(p1, value),
            SetFn::SZeroY => self.set_zero_y(p1, value),
            SetFn::SAbs => self.set_abs(p2, value),
            SetFn::SAbsX => self.set_abs_x(p2, value),
            SetFn::SAbsY => self.set_abs_y(p2, value),
            SetFn::SIndX => self.set_ind_x(p1, value),
            SetFn::SIndY => self.set_ind_y(p1, value),
            _ => {}
        }
    }

    // =========================================================================
    // SECTION J — DO_IRQBRK + DO_INTERRUPT (dtv:314-457).
    // =========================================================================

    // DO_IRQBRK (dtv:314-349): shared IRQ/BRK sequence with NMI transformation.
    fn do_irqbrk(&mut self) {
        let mut handler_vector: u16 = 0xfffe;
        self.push(((self.core.reg_pc >> 8) & 0xff) as u8);
        self.clk_inc();
        self.push((self.core.reg_pc & 0xff) as u8);
        self.clk_inc();
        let st = self.local_status();
        self.push(st);
        self.clk_inc();
        // Process alarms up to this point to get nmi_clk updated.
        self.process_alarms();
        if (self.int.global_pending_int & IK_NMI) != 0
            && (self.core.clk >= self.int.nmi_clk + INTERRUPT_DELAY)
        {
            handler_vector = 0xfffa;
            self.int.interrupt_ack_nmi();
        }
        self.local_set_interrupt(true);
        let mut addr = self.load(handler_vector) as u16;
        self.clk_inc();
        addr |= (self.load(handler_vector.wrapping_add(1)) as u16) << 8;
        self.clk_inc();
        self.jump(addr);
    }

    // BRK (dtv:1009-1017).
    fn brk(&mut self) {
        self.inc_pc(2);
        self.local_set_break(true);
        self.do_irqbrk();
    }

    // DO_INTERRUPT (dtv:354-457): the SC maincpu interrupt dispatch.
    #[allow(unused_assignments)]
    fn do_interrupt(&mut self, int_kind: u32) {
        let mut ik = int_kind;
        let mut addr: u16;

        if ik & (IK_IRQ | IK_IRQPEND | IK_NMI) != 0 {
            let clk = self.core.clk;
            let nmi_now = (ik & IK_NMI) != 0 && interrupt_check_nmi_delay(self.int, clk);
            if nmi_now {
                self.int.interrupt_ack_nmi();
                // !SKIP_CYCLE: two dummy reads of reg_pc.
                self.load_dummy(self.core.reg_pc);
                self.clk_inc();
                self.load_dummy(self.core.reg_pc);
                self.clk_inc();
                self.local_set_break(false);
                self.push(((self.core.reg_pc >> 8) & 0xff) as u8);
                self.clk_inc();
                self.push((self.core.reg_pc & 0xff) as u8);
                self.clk_inc();
                let st = self.local_status();
                self.push(st);
                self.clk_inc();
                addr = self.load(0xfffa) as u16;
                self.clk_inc();
                addr |= (self.load(0xfffb) as u16) << 8;
                self.clk_inc();
                self.local_set_interrupt(true);
                self.jump(addr);
                self.set_last_opcode(0);
            } else {
                // Evaluate the IRQ gate. The DISABLES_IRQ test reads
                // last_opcode_info; the delay check mutates int — sequence them.
                let irq_gate = (ik & (IK_IRQ | IK_IRQPEND)) != 0
                    && (!self.local_interrupt()
                        || opinfo_disables_irq(self.core.last_opcode_info) != 0);
                let irq_now = irq_gate && interrupt_check_irq_delay(self.int, clk);
                if irq_now {
                    self.int.interrupt_ack_irq();
                    self.load_dummy(self.core.reg_pc);
                    self.clk_inc();
                    self.load_dummy(self.core.reg_pc);
                    self.clk_inc();
                    self.local_set_break(false);
                    self.do_irqbrk();
                    self.set_last_opcode(0);
                }
            }
        }

        if ik & (IK_TRAP | IK_RESET) != 0 {
            if ik & IK_TRAP != 0 {
                // trap path; allow IK_RESET to chain (interrupt_do_trap may set it).
                if self.int.global_pending_int & IK_RESET != 0 {
                    ik |= IK_RESET;
                }
            }
            if ik & IK_RESET != 0 {
                self.int.interrupt_ack_reset();
                self.bus.cpu_reset();
                let a = self.load(0xfffc) as u16;
                let a2 = self.load(0xfffd) as u16;
                addr = a | (a2 << 8);
                self.core.bank_start = 0;
                self.core.bank_limit = 0;
                self.local_set_interrupt(true);
                self.core.is_jammed = false;
                self.jump(addr);
            }
        }
        if ik & IK_MONITOR != 0 {
            // Monitor not ported.
        }
    }

    // -------------------------------------------------------------------------
    // JAM_02 (dtv:1022-1040) + JAM (mainc64cpu.c:780-801).
    // -------------------------------------------------------------------------
    // Returns true if execution should continue to the opcode switch (trap
    // handled or replayed), false if it jammed.
    fn jam_02(&mut self) -> bool {
        debug_assert_eq!(TRAP_OPCODE, 0x02);
        let pc = self.core.reg_pc;
        if !self.bus.rom_trap_allowed(pc) {
            self.core.is_jammed = true;
            // REWIND_FETCH_OPCODE is a no-op for SC.
            self.jam_result = self.host_jam();
            return false;
        }
        let trap_result = self.bus.trap_handler();
        if trap_result == 0xffff_ffff {
            self.core.is_jammed = true;
            self.jam_result = self.host_jam();
            return false;
        }
        if trap_result != 0 {
            // SET_OPCODE(trap_result); goto trap_skipped.
            self.trap_opcode = trap_result;
            self.trap_skipped = true;
            return true;
        }
        // trap_result == 0: handled in place, continue normally.
        true
    }

    // PORT OF: mainc64cpu.c:780-801 JAM(). Returns the jam disposition. None ⇒
    // the `default:` path runs CLK_INC() once.
    fn host_jam(&mut self) -> i32 {
        match self.bus.jam() {
            None => {
                self.clk_inc(); // default: CLK_INC().
                JAM_NONE
            }
            Some(JAM_RESET_CPU) => {
                self.do_interrupt(IK_RESET);
                JAM_RESET_CPU
            }
            Some(JAM_POWER_CYCLE) => {
                // machine_powerup() handled by host before returning this code.
                self.do_interrupt(IK_RESET);
                JAM_POWER_CYCLE
            }
            Some(JAM_MONITOR) => JAM_MONITOR,
            Some(code) => code,
        }
    }
}

// GetFn / SetFn — the get_func/set_func the opcode switch passes to the body
// macros (dtv addressing modes). LdDest selects LD's target register.
#[derive(Clone, Copy)]
enum GetFn {
    Imm,
    Zero,
    ZeroX,
    ZeroY,
    Abs,
    AbsX,
    AbsY,
    AbsXRmw,
    AbsYRmw,
    IndX,
    IndY,
    IndYRmw,
}
// SetFn variants are S-prefixed so a single `use GetFn::*` glob import in the
// decode table is unambiguous (GetFn and SetFn share many mode names).
#[derive(Clone, Copy)]
enum SetFn {
    SZero,
    SZeroX,
    SZeroY,
    SZeroRmw,
    SZeroXRmw,
    SAbs,
    SAbsX,
    SAbsY,
    SAbsRmw,
    SAbsXRmw,
    SAbsYRmw,
    SIndX,
    SIndY,
    SIndRmw,
}
#[derive(Clone, Copy)]
enum LdDest {
    A,
    X,
    Y,
}

// =============================================================================
// SECTION K — the entry point: c64_6510core_execute.
//
// ONE call advances the C64 CPU by exactly one opcode (or one interrupt
// dispatch), updating core.clk per-cycle via CLK_INC. PORT OF: the SC mainloop
// body (dtv:1714-2798), the prologue (alarms + jam HACK + pending-interrupt
// dispatch) and FETCH_OPCODE (c64cpusc.c:152-179) + the opcode switch.
//
// Return value: JAM_NONE on a normal opcode (or IRQ/NMI/RESET dispatch), or one
// of JAM_RESET_CPU / JAM_POWER_CYCLE / JAM_MONITOR on a JAM.
// =============================================================================
pub fn c64_6510core_execute<B: C64Core6510Bus>(
    core: &mut C64Core6510,
    bus: &mut B,
    int: &mut IntStatus,
) -> i32 {
    // Keep the OPINFO word mirrored into int at entry.
    int.last_opcode_info = core.last_opcode_info;

    let mut ex = Exec {
        core,
        bus,
        int,
        jam_result: JAM_NONE,
        trap_skipped: false,
        trap_opcode: 0,
        pending_ind_addr: 0,
    };
    run(&mut ex)
}

/// The SC mainloop body (one instruction). Split out so the Exec borrow is a
/// clean &mut. PORT OF: dtv:1714-2798 (prologue + fetch + switch).
fn run<B: C64Core6510Bus>(ex: &mut Exec<B>) -> i32 {
    {
        // --- Prologue (dtv:1734-1772) ---

        // 1) alarm dispatch up to clk.
        ex.process_alarms();

        // 2) HACK: jammed CPU clears IRQ/NMI + only RESET wakes (dtv:1741-1747).
        if ex.core.is_jammed {
            ex.int.interrupt_ack_irq();
            ex.int.global_pending_int &= !(IK_IRQ | IK_NMI);
            if ex.int.global_pending_int & IK_RESET != 0 {
                ex.core.is_jammed = false;
            }
        }

        // 3) Pending-interrupt dispatch (dtv:1749-1772).
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

        // --- FETCH (dtv:1792-1812 + c64cpusc.c:152-179) ---
        ex.set_last_addr(ex.core.reg_pc);

        let (p0, p1, p2) = fetch_opcode(ex);

        // OPCODE_UPDATE_IN_FETCH is defined → SET_LAST_OPCODE happened in FETCH.
        // For jammed CPU, VICE re-forces lastop; our FETCH already set it.

        // Tracing hook (dtv:1822-1833).
        {
            let pc = ex.core.reg_pc;
            let clk = ex.core.clk;
            ex.bus.debug_maincpu(pc, clk, p0, p1, ((p2 >> 8) & 0xff) as u8);
        }

        // --- The opcode switch (dtv:1845-2790). VERBATIM, case-per-opcode. ---
        // The trap_skipped re-entry (dtv:1840) is handled inside case 0x02 of
        // decode_and_execute (where JAM_02 lives), mirroring the C `goto`.
        decode_and_execute(ex, p0, p1, p2);

        ex.jam_result
    }
}

/// PORT OF: c64cpusc.c:152-179 FETCH_OPCODE (the !BIGENDIAN/else per-byte path,
/// which is what runs without ALLOW_UNALIGNED_ACCESS or with the union opcode_t).
/// Each byte fetch runs check_ba()+CLK_INC. SET_LAST_OPCODE(p0) happens after the
/// first byte (OPCODE_UPDATE_IN_FETCH). Returns (p0, p1, p2).
fn fetch_opcode<B: C64Core6510Bus>(ex: &mut Exec<B>) -> (u8, u8, u16) {
    let reg_pc = ex.core.reg_pc;
    // Bank fast-path (reg_pc < bank_limit) only when a flat window is pinned.
    if (reg_pc as i32) < ex.core.bank_limit && ex.core.bank_base.is_some() {
        ex.check_ba();
        let base = ex.core.bank_base.as_ref().unwrap();
        let ins = base[reg_pc as usize];
        ex.set_last_opcode(ins as u32);
        ex.clk_inc();
        ex.check_ba();
        let lo = ex.core.bank_base.as_ref().unwrap()[reg_pc.wrapping_add(1) as usize];
        let mut p2 = lo as u16;
        ex.clk_inc();
        if FETCH_TAB[ins as usize] != 0 {
            ex.check_ba();
            let hi = ex.core.bank_base.as_ref().unwrap()[reg_pc.wrapping_add(2) as usize];
            p2 |= (hi as u16) << 8;
            ex.clk_inc();
        }
        (ins, lo, p2)
    } else {
        // Per-byte FETCH path (FETCH_OPCODE — the separate fetch read tab).
        let ins = ex.load_fetch(reg_pc);
        ex.set_last_opcode(ins as u32);
        ex.clk_inc();
        let lo = ex.load_fetch(reg_pc.wrapping_add(1));
        let mut p2 = lo as u16;
        ex.clk_inc();
        if FETCH_TAB[ins as usize] != 0 {
            let hi = ex.load_fetch(reg_pc.wrapping_add(2));
            p2 |= (hi as u16) << 8;
            ex.clk_inc();
        }
        (ins, lo, p2)
    }
}

impl<'a, B: C64Core6510Bus> Exec<'a, B> {
    // SET_OPCODE replay path (after a trap): rewrite last_opcode_info to the new
    // opcode (the operand bytes p1/p2 are reread by the body via load if needed;
    // VICE's SET_OPCODE only sets the opcode_t, the operands are already fetched).
    #[inline]
    fn set_opcode_replay(&mut self, o: u32) {
        // OPCODE_UPDATE_IN_FETCH means SET_LAST_OPCODE was driven by FETCH; on a
        // trap replay VICE calls SET_OPCODE then re-enters at trap_skipped, where
        // (since OPCODE_UPDATE_IN_FETCH) the SET_LAST_OPCODE is NOT re-issued.
        // We keep last_opcode_info as the replayed opcode number.
        self.core.last_opcode_info = (o & 0xff) | (self.core.last_opcode_info & !0xff);
        self.int.last_opcode_info = self.core.last_opcode_info;
    }
}

/// PORT OF: the dtv opcode switch (dtv:1845-2790), NON-DTV path. Every opcode is
/// its own case (illegals fully cycle-stepped via the same get/set helpers).
fn decode_and_execute<B: C64Core6510Bus>(ex: &mut Exec<B>, p0: u8, p1: u8, p2: u16) {
    use GetFn::*;
    use LdDest::*;
    use SetFn::*; // S-prefixed variants — no collision with GetFn glob.
    match p0 {
        0x00 => ex.brk(),                              // BRK
        0x01 => ex.ora_g(p1, p2, 2, IndX),             // ORA ($nn,X)
        0x02 => {
            // JAM - also used for traps. dtv:1854-1857.
            if ex.jam_02() && ex.trap_skipped {
                // Trap replaced the opcode → SET_OPCODE(trap_result); goto
                // trap_skipped: re-enter the switch with the new opcode. The
                // operand bytes p1/p2 are unchanged (VICE keeps them). dtv:1031-1035.
                ex.trap_skipped = false;
                let t = ex.trap_opcode;
                ex.set_opcode_replay(t);
                decode_and_execute(ex, (t & 0xff) as u8, p1, p2);
            }
            // else: real JAM (jam_result set) or trap handled in place — done.
        }
        0x22 | 0x52 | 0x62 | 0x72 | 0x92 | 0xb2 | 0xd2 | 0xf2 | 0x12 | 0x32 | 0x42 => {
            // JAM (dtv:1859-1875). The DTV-only 0x12/0x32/0x42 fall here too in
            // the non-DTV build (they are NOT BRA/SAC/SIR without C64DTV).
            ex.core.is_jammed = true;
            // REWIND_FETCH_OPCODE no-op for SC.
            ex.jam_result = ex.host_jam();
        }

        0x03 => ex.slo(p1, p2, 2, IndX, SIndRmw),       // SLO ($nn,X)
        0x04 | 0x44 | 0x64 => ex.noop(p1, p2, 2, Zero), // NOOP $nn (GET_ZERO_DUMMY)
        0x05 => ex.ora_g(p1, p2, 2, Zero),             // ORA $nn
        0x06 => ex.asl(p1, p2, 2, Zero, SZeroRmw),      // ASL $nn
        0x07 => ex.slo(p1, p2, 2, Zero, SZeroRmw),      // SLO $nn
        0x08 => ex.php(),                              // PHP
        0x09 => ex.ora_g(p1, p2, 2, Imm),              // ORA #$nn
        0x0a => ex.asl_a(),                            // ASL A
        0x0b | 0x2b => ex.anc(p1),                     // ANC #$nn
        0x0c => ex.noop(p1, p2, 3, Abs),               // NOOP $nnnn (GET_ABS_DUMMY)
        0x0d => ex.ora_g(p1, p2, 3, Abs),              // ORA $nnnn
        0x0e => ex.asl(p1, p2, 3, Abs, SAbsRmw),        // ASL $nnnn
        0x0f => ex.slo(p1, p2, 3, Abs, SAbsRmw),        // SLO $nnnn

        0x10 => ex.branch(!ex.local_sign(), p1),       // BPL
        0x11 => ex.ora_g(p1, p2, 2, IndY),             // ORA ($nn),Y
        0x13 => ex.slo(p1, p2, 2, IndYRmw, SIndRmw),    // SLO ($nn),Y
        0x14 | 0x34 | 0x54 | 0x74 | 0xd4 | 0xf4 => ex.noop(p1, p2, 2, ZeroX), // NOOP $nn,X
        0x15 => ex.ora_g(p1, p2, 2, ZeroX),            // ORA $nn,X
        0x16 => ex.asl(p1, p2, 2, ZeroX, SZeroXRmw),    // ASL $nn,X
        0x17 => ex.slo(p1, p2, 2, ZeroX, SZeroXRmw),    // SLO $nn,X
        0x18 => ex.clc(),                              // CLC
        0x19 => ex.ora_g(p1, p2, 3, AbsY),             // ORA $nnnn,Y
        0x1a | 0x3a | 0x5a | 0x7a | 0xda | 0xfa | 0xea => ex.noop(p1, p2, 1, Imm), // NOP/NOOP
        0x1b => ex.slo(p1, p2, 3, AbsYRmw, SAbsYRmw),   // SLO $nnnn,Y
        0x1c | 0x3c | 0x5c | 0x7c | 0xdc | 0xfc => ex.noop(p1, p2, 3, AbsX), // NOOP $nnnn,X
        0x1d => ex.ora_g(p1, p2, 3, AbsX),             // ORA $nnnn,X
        0x1e => ex.asl(p1, p2, 3, AbsXRmw, SAbsXRmw),   // ASL $nnnn,X
        0x1f => ex.slo(p1, p2, 3, AbsXRmw, SAbsXRmw),   // SLO $nnnn,X

        0x20 => ex.jsr(p1),                            // JSR $nnnn
        0x21 => ex.and_g(p1, p2, 2, IndX),             // AND ($nn,X)
        0x23 => ex.rla(p1, p2, 2, IndX, SIndRmw),       // RLA ($nn,X)
        0x24 => ex.bit(p1, p2, 2, Zero),               // BIT $nn
        0x25 => ex.and_g(p1, p2, 2, Zero),             // AND $nn
        0x26 => ex.rol(p1, p2, 2, Zero, SZeroRmw),      // ROL $nn
        0x27 => ex.rla(p1, p2, 2, Zero, SZeroRmw),      // RLA $nn
        0x28 => ex.plp(),                              // PLP
        0x29 => ex.and_g(p1, p2, 2, Imm),              // AND #$nn
        0x2a => ex.rol_a(),                            // ROL A
        0x2c => ex.bit(p1, p2, 3, Abs),                // BIT $nnnn
        0x2d => ex.and_g(p1, p2, 3, Abs),              // AND $nnnn
        0x2e => ex.rol(p1, p2, 3, Abs, SAbsRmw),        // ROL $nnnn
        0x2f => ex.rla(p1, p2, 3, Abs, SAbsRmw),        // RLA $nnnn

        0x30 => ex.branch(ex.local_sign(), p1),        // BMI
        0x31 => ex.and_g(p1, p2, 2, IndY),             // AND ($nn),Y
        0x33 => ex.rla(p1, p2, 2, IndYRmw, SIndRmw),    // RLA ($nn),Y
        0x35 => ex.and_g(p1, p2, 2, ZeroX),            // AND $nn,X
        0x36 => ex.rol(p1, p2, 2, ZeroX, SZeroXRmw),    // ROL $nn,X
        0x37 => ex.rla(p1, p2, 2, ZeroX, SZeroXRmw),    // RLA $nn,X
        0x38 => ex.sec(),                              // SEC
        0x39 => ex.and_g(p1, p2, 3, AbsY),             // AND $nnnn,Y
        0x3b => ex.rla(p1, p2, 3, AbsYRmw, SAbsYRmw),   // RLA $nnnn,Y
        0x3d => ex.and_g(p1, p2, 3, AbsX),             // AND $nnnn,X
        0x3e => ex.rol(p1, p2, 3, AbsXRmw, SAbsXRmw),   // ROL $nnnn,X
        0x3f => ex.rla(p1, p2, 3, AbsXRmw, SAbsXRmw),   // RLA $nnnn,X

        0x40 => ex.rti(),                              // RTI
        0x41 => ex.eor_g(p1, p2, 2, IndX),             // EOR ($nn,X)
        0x43 => ex.sre(p1, p2, 2, IndX, SIndRmw),       // SRE ($nn,X)
        0x45 => ex.eor_g(p1, p2, 2, Zero),             // EOR $nn
        0x46 => ex.lsr(p1, p2, 2, Zero, SZeroRmw),      // LSR $nn
        0x47 => ex.sre(p1, p2, 2, Zero, SZeroRmw),      // SRE $nn
        0x48 => ex.pha(),                              // PHA
        0x49 => ex.eor_g(p1, p2, 2, Imm),              // EOR #$nn
        0x4a => ex.lsr_a(),                            // LSR A
        0x4b => ex.asr(p1),                            // ASR #$nn
        0x4c => ex.jmp(p2),                            // JMP $nnnn
        0x4d => ex.eor_g(p1, p2, 3, Abs),              // EOR $nnnn
        0x4e => ex.lsr(p1, p2, 3, Abs, SAbsRmw),        // LSR $nnnn
        0x4f => ex.sre(p1, p2, 3, Abs, SAbsRmw),        // SRE $nnnn

        0x50 => ex.branch(!ex.local_overflow(), p1),   // BVC
        0x51 => ex.eor_g(p1, p2, 2, IndY),             // EOR ($nn),Y
        0x53 => ex.sre(p1, p2, 2, IndYRmw, SIndRmw),    // SRE ($nn),Y
        0x55 => ex.eor_g(p1, p2, 2, ZeroX),            // EOR $nn,X
        0x56 => ex.lsr(p1, p2, 2, ZeroX, SZeroXRmw),    // LSR $nn,X
        0x57 => ex.sre(p1, p2, 2, ZeroX, SZeroXRmw),    // SRE $nn,X
        0x58 => ex.cli(),                              // CLI
        0x59 => ex.eor_g(p1, p2, 3, AbsY),             // EOR $nnnn,Y
        0x5b => ex.sre(p1, p2, 3, AbsYRmw, SAbsYRmw),   // SRE $nnnn,Y
        0x5d => ex.eor_g(p1, p2, 3, AbsX),             // EOR $nnnn,X
        0x5e => ex.lsr(p1, p2, 3, AbsXRmw, SAbsXRmw),   // LSR $nnnn,X
        0x5f => ex.sre(p1, p2, 3, AbsXRmw, SAbsXRmw),   // SRE $nnnn,X

        0x60 => ex.rts(),                              // RTS
        0x61 => ex.adc_g(p1, p2, 2, IndX),             // ADC ($nn,X)
        0x63 => ex.rra(p1, p2, 2, IndX, SIndRmw),       // RRA ($nn,X)
        0x65 => ex.adc_g(p1, p2, 2, Zero),             // ADC $nn
        0x66 => ex.ror(p1, p2, 2, Zero, SZeroRmw),      // ROR $nn
        0x67 => ex.rra(p1, p2, 2, Zero, SZeroRmw),      // RRA $nn
        0x68 => ex.pla(),                              // PLA
        0x69 => ex.adc_g(p1, p2, 2, Imm),              // ADC #$nn
        0x6a => ex.ror_a(),                            // ROR A
        0x6b => ex.arr(p1),                            // ARR #$nn
        0x6c => ex.jmp_ind(p2),                        // JMP ($nnnn)
        0x6d => ex.adc_g(p1, p2, 3, Abs),              // ADC $nnnn
        0x6e => ex.ror(p1, p2, 3, Abs, SAbsRmw),        // ROR $nnnn
        0x6f => ex.rra(p1, p2, 3, Abs, SAbsRmw),        // RRA $nnnn

        0x70 => ex.branch(ex.local_overflow(), p1),    // BVS
        0x71 => ex.adc_g(p1, p2, 2, IndY),             // ADC ($nn),Y
        0x73 => ex.rra(p1, p2, 2, IndYRmw, SIndRmw),    // RRA ($nn),Y
        0x75 => ex.adc_g(p1, p2, 2, ZeroX),            // ADC $nn,X
        0x76 => ex.ror(p1, p2, 2, ZeroX, SZeroXRmw),    // ROR $nn,X
        0x77 => ex.rra(p1, p2, 2, ZeroX, SZeroXRmw),    // RRA $nn,X
        0x78 => ex.sei(),                              // SEI
        0x79 => ex.adc_g(p1, p2, 3, AbsY),             // ADC $nnnn,Y
        0x7b => ex.rra(p1, p2, 3, AbsYRmw, SAbsYRmw),   // RRA $nnnn,Y
        0x7d => ex.adc_g(p1, p2, 3, AbsX),             // ADC $nnnn,X
        0x7e => ex.ror(p1, p2, 3, AbsXRmw, SAbsXRmw),   // ROR $nnnn,X
        0x7f => ex.rra(p1, p2, 3, AbsXRmw, SAbsXRmw),   // RRA $nnnn,X

        0x80 | 0x82 | 0x89 | 0xc2 | 0xe2 => ex.noop(p1, p2, 2, Imm), // NOOP #$nn (GET_IMM_DUMMY)
        0x81 => ex.st(ex.core.reg_a, p1, p2, 2, SIndX), // STA ($nn,X)
        0x83 => ex.st(ex.core.reg_a & ex.core.reg_x, p1, p2, 2, SIndX), // SAX ($nn,X)
        0x84 => ex.st(ex.core.reg_y, p1, p2, 2, SZero), // STY $nn
        0x85 => ex.st(ex.core.reg_a, p1, p2, 2, SZero), // STA $nn
        0x86 => ex.st(ex.core.reg_x, p1, p2, 2, SZero), // STX $nn
        0x87 => ex.st(ex.core.reg_a & ex.core.reg_x, p1, p2, 2, SZero), // SAX $nn
        0x88 => ex.dey(),                              // DEY
        0x8a => ex.txa(),                              // TXA
        0x8b => ex.ane(p1),                            // ANE #$nn
        0x8c => ex.st(ex.core.reg_y, p1, p2, 3, SAbs),  // STY $nnnn
        0x8d => ex.st(ex.core.reg_a, p1, p2, 3, SAbs),  // STA $nnnn
        0x8e => ex.st(ex.core.reg_x, p1, p2, 3, SAbs),  // STX $nnnn
        0x8f => ex.st(ex.core.reg_a & ex.core.reg_x, p1, p2, 3, SAbs), // SAX $nnnn

        0x90 => ex.branch(!ex.local_carry(), p1),      // BCC
        0x91 => ex.st(ex.core.reg_a, p1, p2, 2, SIndY), // STA ($nn),Y
        0x93 => ex.sha_ind_y(p1),                      // SHA ($nn),Y
        0x94 => ex.st(ex.core.reg_y, p1, p2, 2, SZeroX), // STY $nn,X
        0x95 => ex.st(ex.core.reg_a, p1, p2, 2, SZeroX), // STA $nn,X
        0x96 => ex.st(ex.core.reg_x, p1, p2, 2, SZeroY), // STX $nn,Y
        0x97 => ex.st(ex.core.reg_a & ex.core.reg_x, p1, p2, 2, SZeroY), // SAX $nn,Y
        0x98 => ex.tya(),                              // TYA
        0x99 => ex.st(ex.core.reg_a, p1, p2, 3, SAbsY), // STA $nnnn,Y
        0x9a => ex.txs(),                              // TXS
        0x9b => ex.shs_abs_y(p2),                      // SHS $nnnn,Y (non-DTV)
        0x9c => ex.sh_abs_i(p2, ex.core.reg_y, ex.core.reg_x), // SHY $nnnn,X
        0x9d => ex.st(ex.core.reg_a, p1, p2, 3, SAbsX), // STA $nnnn,X
        0x9e => ex.sh_abs_i(p2, ex.core.reg_x, ex.core.reg_y), // SHX $nnnn,Y
        0x9f => ex.sh_abs_i(p2, ex.core.reg_a & ex.core.reg_x, ex.core.reg_y), // SHA $nnnn,Y

        0xa0 => ex.ld(Y, p1, p2, 2, Imm),              // LDY #$nn
        0xa1 => ex.ld(A, p1, p2, 2, IndX),             // LDA ($nn,X)
        0xa2 => ex.ld(X, p1, p2, 2, Imm),              // LDX #$nn
        0xa3 => ex.lax(p1, p2, 2, IndX),               // LAX ($nn,X)
        0xa4 => ex.ld(Y, p1, p2, 2, Zero),             // LDY $nn
        0xa5 => ex.ld(A, p1, p2, 2, Zero),             // LDA $nn
        0xa6 => ex.ld(X, p1, p2, 2, Zero),             // LDX $nn
        0xa7 => ex.lax(p1, p2, 2, Zero),               // LAX $nn
        0xa8 => ex.tay(),                              // TAY
        0xa9 => ex.ld(A, p1, p2, 2, Imm),              // LDA #$nn
        0xaa => ex.tax(),                              // TAX
        0xab => ex.lxa(p1),                            // LXA #$nn
        0xac => ex.ld(Y, p1, p2, 3, Abs),              // LDY $nnnn
        0xad => ex.ld(A, p1, p2, 3, Abs),              // LDA $nnnn
        0xae => ex.ld(X, p1, p2, 3, Abs),              // LDX $nnnn
        0xaf => ex.lax(p1, p2, 3, Abs),                // LAX $nnnn

        0xb0 => ex.branch(ex.local_carry(), p1),       // BCS
        0xb1 => ex.ld(A, p1, p2, 2, IndY),             // LDA ($nn),Y
        0xb3 => ex.lax(p1, p2, 2, IndY),               // LAX ($nn),Y
        0xb4 => ex.ld(Y, p1, p2, 2, ZeroX),            // LDY $nn,X
        0xb5 => ex.ld(A, p1, p2, 2, ZeroX),            // LDA $nn,X
        0xb6 => ex.ld(X, p1, p2, 2, ZeroY),            // LDX $nn,Y
        0xb7 => ex.lax(p1, p2, 2, ZeroY),              // LAX $nn,Y
        0xb8 => ex.clv(),                              // CLV
        0xb9 => ex.ld(A, p1, p2, 3, AbsY),             // LDA $nnnn,Y
        0xba => ex.tsx(),                              // TSX
        0xbb => ex.las(p2),                            // LAS $nnnn,Y
        0xbc => ex.ld(Y, p1, p2, 3, AbsX),             // LDY $nnnn,X
        0xbd => ex.ld(A, p1, p2, 3, AbsX),             // LDA $nnnn,X
        0xbe => ex.ld(X, p1, p2, 3, AbsY),             // LDX $nnnn,Y
        0xbf => ex.lax(p1, p2, 3, AbsY),               // LAX $nnnn,Y

        0xc0 => ex.cp_g(ex.core.reg_y, p1, p2, 2, Imm), // CPY #$nn
        0xc1 => ex.cp_g(ex.core.reg_a, p1, p2, 2, IndX), // CMP ($nn,X)
        0xc3 => ex.dcp(p1, p2, 2, IndX, SIndRmw),       // DCP ($nn,X)
        0xc4 => ex.cp_g(ex.core.reg_y, p1, p2, 2, Zero), // CPY $nn
        0xc5 => ex.cp_g(ex.core.reg_a, p1, p2, 2, Zero), // CMP $nn
        0xc6 => ex.dec(p1, p2, 2, Zero, SZeroRmw),      // DEC $nn
        0xc7 => ex.dcp(p1, p2, 2, Zero, SZeroRmw),      // DCP $nn
        0xc8 => ex.iny(),                              // INY
        0xc9 => ex.cp_g(ex.core.reg_a, p1, p2, 2, Imm), // CMP #$nn
        0xca => ex.dex(),                              // DEX
        0xcb => ex.sbx(p1),                            // SBX #$nn
        0xcc => ex.cp_g(ex.core.reg_y, p1, p2, 3, Abs), // CPY $nnnn
        0xcd => ex.cp_g(ex.core.reg_a, p1, p2, 3, Abs), // CMP $nnnn
        0xce => ex.dec(p1, p2, 3, Abs, SAbsRmw),        // DEC $nnnn
        0xcf => ex.dcp(p1, p2, 3, Abs, SAbsRmw),        // DCP $nnnn

        0xd0 => ex.branch(!ex.local_zero(), p1),       // BNE
        0xd1 => ex.cp_g(ex.core.reg_a, p1, p2, 2, IndY), // CMP ($nn),Y
        0xd3 => ex.dcp(p1, p2, 2, IndYRmw, SIndRmw),    // DCP ($nn),Y
        0xd5 => ex.cp_g(ex.core.reg_a, p1, p2, 2, ZeroX), // CMP $nn,X
        0xd6 => ex.dec(p1, p2, 2, ZeroX, SZeroXRmw),    // DEC $nn,X
        0xd7 => ex.dcp(p1, p2, 2, ZeroX, SZeroXRmw),    // DCP $nn,X
        0xd8 => ex.cld(),                              // CLD
        0xd9 => ex.cp_g(ex.core.reg_a, p1, p2, 3, AbsY), // CMP $nnnn,Y
        0xdb => ex.dcp(p1, p2, 3, AbsYRmw, SAbsYRmw),   // DCP $nnnn,Y
        0xdd => ex.cp_g(ex.core.reg_a, p1, p2, 3, AbsX), // CMP $nnnn,X
        0xde => ex.dec(p1, p2, 3, AbsXRmw, SAbsXRmw),   // DEC $nnnn,X
        0xdf => ex.dcp(p1, p2, 3, AbsXRmw, SAbsXRmw),   // DCP $nnnn,X

        0xe0 => ex.cp_g(ex.core.reg_x, p1, p2, 2, Imm), // CPX #$nn
        0xe1 => ex.sbc_g(p1, p2, 2, IndX),             // SBC ($nn,X)
        0xe3 => ex.isb(p1, p2, 2, IndX, SIndRmw),       // ISB ($nn,X)
        0xe4 => ex.cp_g(ex.core.reg_x, p1, p2, 2, Zero), // CPX $nn
        0xe5 => ex.sbc_g(p1, p2, 2, Zero),             // SBC $nn
        0xe6 => ex.inc(p1, p2, 2, Zero, SZeroRmw),      // INC $nn
        0xe7 => ex.isb(p1, p2, 2, Zero, SZeroRmw),      // ISB $nn
        0xe8 => ex.inx(),                              // INX
        0xe9 | 0xeb => ex.sbc_g(p1, p2, 2, Imm),       // SBC / USBC #$nn
        0xec => ex.cp_g(ex.core.reg_x, p1, p2, 3, Abs), // CPX $nnnn
        0xed => ex.sbc_g(p1, p2, 3, Abs),              // SBC $nnnn
        0xee => ex.inc(p1, p2, 3, Abs, SAbsRmw),        // INC $nnnn
        0xef => ex.isb(p1, p2, 3, Abs, SAbsRmw),        // ISB $nnnn

        0xf0 => ex.branch(ex.local_zero(), p1),        // BEQ
        0xf1 => ex.sbc_g(p1, p2, 2, IndY),             // SBC ($nn),Y
        0xf3 => ex.isb(p1, p2, 2, IndYRmw, SIndRmw),    // ISB ($nn),Y
        0xf5 => ex.sbc_g(p1, p2, 2, ZeroX),            // SBC $nn,X
        0xf6 => ex.inc(p1, p2, 2, ZeroX, SZeroXRmw),    // INC $nn,X
        0xf7 => ex.isb(p1, p2, 2, ZeroX, SZeroXRmw),    // ISB $nn,X
        0xf8 => ex.sed(),                              // SED
        0xf9 => ex.sbc_g(p1, p2, 3, AbsY),             // SBC $nnnn,Y
        0xfb => ex.isb(p1, p2, 3, AbsYRmw, SAbsYRmw),   // ISB $nnnn,Y
        0xfd => ex.sbc_g(p1, p2, 3, AbsX),             // SBC $nnnn,X
        0xfe => ex.inc(p1, p2, 3, AbsXRmw, SAbsXRmw),   // INC $nnnn,X
        0xff => ex.isb(p1, p2, 3, AbsXRmw, SAbsXRmw),   // ISB $nnnn,X
    }
}

// =============================================================================
// SECTION L — #[cfg(test)] smoke test.
//
// A flat 64 KB RAM bus implementing C64Core6510Bus with a no-op VIC (vic_cycle
// records nothing, check_ba steals 0 cycles). Proves: a few normal instructions
// execute with the right register/PC/cycle results, and the SHX ($9e) illegal
// opcode is fully cycle-stepped and models the SET_ABS_SH_I value + the mispaged
// BA-low dummy read + the page-cross target — the exact gap the audit found in
// the microcode-pattern cpu.rs.
// =============================================================================
#[cfg(test)]
mod tests {
    use super::*;

    /// Flat-RAM test bus. Records the sequence of (kind, addr) accesses so the
    /// SH* dummy-read + final store can be asserted at the exact cycle/address.
    struct RamBus {
        ram: Box<[u8; 0x10000]>,
        /// ('R'/'r'(dummy)/'W'/'w'(dummy), addr) trace.
        trace: Vec<(char, u16, u8)>,
    }
    impl RamBus {
        fn new() -> Self {
            RamBus { ram: Box::new([0u8; 0x10000]), trace: Vec::new() }
        }
    }
    impl C64Core6510Bus for RamBus {
        fn read_raw(&mut self, a: u16) -> u8 {
            let v = self.ram[a as usize];
            self.trace.push(('R', a, v));
            v
        }
        fn write_raw(&mut self, a: u16, v: u8) {
            self.ram[a as usize] = v;
            self.trace.push(('W', a, v));
        }
        fn read_raw_dummy(&mut self, a: u16) -> u8 {
            let v = self.ram[a as usize];
            self.trace.push(('r', a, v));
            v
        }
        fn write_raw_dummy(&mut self, a: u16, v: u8) {
            // VICE STORE_DUMMY writes the (old) value to the address (the dummy
            // RMW write-back), then the real STORE follows. For flat RAM the net
            // effect is old-then-new = the new value.
            self.ram[a as usize] = v;
            self.trace.push(('w', a, v));
        }
        // No VIC steal in the test.
        fn check_ba(&mut self, _loi: &mut u32, _ba_low: bool) -> u64 {
            0
        }
        fn vic_cycle(&mut self, _clk: u64) {}
    }

    /// Run exactly one instruction.
    fn step(core: &mut C64Core6510, bus: &mut RamBus, int: &mut IntStatus) -> i32 {
        bus.trace.clear();
        c64_6510core_execute(core, bus, int)
    }

    #[test]
    fn smoke_basic_instructions() {
        let mut core = C64Core6510::new();
        let mut bus = RamBus::new();
        let mut int = IntStatus::new();

        // Program at $1000:  A2 42      LDX #$42
        //                    8E 00 20   STX $2000
        //                    E8         INX
        bus.ram[0x1000] = 0xa2;
        bus.ram[0x1001] = 0x42;
        bus.ram[0x1002] = 0x8e;
        bus.ram[0x1003] = 0x00;
        bus.ram[0x1004] = 0x20;
        bus.ram[0x1005] = 0xe8;
        core.reg_pc = 0x1000;
        let clk0 = core.clk;

        // LDX #$42 — 2 cycles.
        step(&mut core, &mut bus, &mut int);
        assert_eq!(core.reg_x, 0x42, "LDX loaded X");
        assert_eq!(core.reg_pc, 0x1002, "LDX advanced PC by 2");
        assert_eq!(core.clk - clk0, 2, "LDX took 2 cycles");
        assert_eq!(core.flag_n, 0x42, "N flag cache = value");
        let clk1 = core.clk;

        // STX $2000 — 4 cycles, stores $42.
        step(&mut core, &mut bus, &mut int);
        assert_eq!(bus.ram[0x2000], 0x42, "STX stored X to $2000");
        assert_eq!(core.reg_pc, 0x1005, "STX advanced PC by 3");
        assert_eq!(core.clk - clk1, 4, "STX abs took 4 cycles");
        let clk2 = core.clk;

        // INX — 2 cycles, X becomes $43.
        step(&mut core, &mut bus, &mut int);
        assert_eq!(core.reg_x, 0x43, "INX incremented X");
        assert_eq!(core.clk - clk2, 2, "INX took 2 cycles");
    }

    /// SHX $nnnn,Y ($9e) — the illegal opcode the audit flagged. Proves the
    /// SET_ABS_SH_I model: value = X & ((addr_hi)+1), the mispaged BA-low dummy
    /// read at ((addr+Y)&0xff)|(addr&0xff00), and the page-cross target.
    #[test]
    fn smoke_shx_page_cross_corruption() {
        let mut core = C64Core6510::new();
        let mut bus = RamBus::new();
        let mut int = IntStatus::new();

        // 9E FF 30   SHX $30FF,Y   with Y = $02, X = $ff.
        // addr = $30FF; addr+Y = $3101 (page cross $30 -> $31).
        // value stored = X & ((addr>>8)+1) = $ff & ($30+1=$31) = $31.
        // page-cross dummy read at ((addr+Y)&0xff)|(addr&0xff00) = $01 | $3000 = $3001.
        // target = ((addr+Y)&0xff) | (value<<8) = $01 | ($31<<8) = $3101.
        bus.ram[0x1000] = 0x9e;
        bus.ram[0x1001] = 0xff;
        bus.ram[0x1002] = 0x30;
        core.reg_pc = 0x1000;
        core.reg_x = 0xff;
        core.reg_y = 0x02;
        let clk0 = core.clk;

        step(&mut core, &mut bus, &mut int);

        // 5 cycles: opcode + 2 operand fetches + BA-low dummy + store.
        assert_eq!(core.clk - clk0, 5, "SHX abs,Y took 5 cycles");
        assert_eq!(core.reg_pc, 0x1003, "SHX advanced PC by 3");

        // The mispaged BA-low dummy read happened at $3001 (NOT $3101).
        let dummy = bus.trace.iter().find(|(k, _, _)| *k == 'r' || *k == 'R');
        let dummy_addr = bus
            .trace
            .iter()
            .filter(|(k, _, _)| *k == 'r')
            .map(|(_, a, _)| *a)
            .last()
            .expect("SHX emitted a dummy read");
        assert_eq!(dummy_addr, 0x3001, "SHX mispaged BA-low dummy read at $3001");
        let _ = dummy;

        // The final store: value $31 at target $3101.
        let store = bus
            .trace
            .iter()
            .find(|(k, _, _)| *k == 'W')
            .expect("SHX emitted a store");
        assert_eq!(store.1, 0x3101, "SHX target = $3101 (page-cross carried)");
        assert_eq!(store.2, 0x31, "SHX value = X & ((addr_hi)+1) = $31");
        assert_eq!(bus.ram[0x3101], 0x31, "SHX wrote $31 to $3101");
    }

    /// SHX with NO page cross — value still X & (hi+1), target is the plain
    /// addr+Y, and the dummy read sits in the SAME page (no corruption visible
    /// in the target, only the AND in the value).
    #[test]
    fn smoke_shx_no_page_cross() {
        let mut core = C64Core6510::new();
        let mut bus = RamBus::new();
        let mut int = IntStatus::new();

        // 9E 00 30   SHX $3000,Y   Y=$05, X=$ff.
        // addr = $3000; addr+Y = $3005 (no cross). value = $ff & $31 = $31.
        // target = $3005. dummy read at ($05)|($3000) = $3005.
        bus.ram[0x1000] = 0x9e;
        bus.ram[0x1001] = 0x00;
        bus.ram[0x1002] = 0x30;
        core.reg_pc = 0x1000;
        core.reg_x = 0xff;
        core.reg_y = 0x05;

        step(&mut core, &mut bus, &mut int);

        let store = bus.trace.iter().find(|(k, _, _)| *k == 'W').unwrap();
        assert_eq!(store.1, 0x3005, "no-cross target = $3005");
        assert_eq!(store.2, 0x31, "no-cross value = X & (hi+1) = $31");
    }

    /// A taken branch emits the dummy read of reg_pc and (on no page cross) sets
    /// DELAYS_INTERRUPT — the branch-dummy gap the audit found.
    #[test]
    fn smoke_branch_dummy_and_delay() {
        let mut core = C64Core6510::new();
        let mut bus = RamBus::new();
        let mut int = IntStatus::new();

        // At $1000: F0 02  BEQ +2  (taken, no page cross -> dummy + DELAYS_INTERRUPT).
        bus.ram[0x1000] = 0xf0;
        bus.ram[0x1001] = 0x02;
        core.reg_pc = 0x1000;
        core.flag_z = 0; // Z set (LOCAL_ZERO() true) so BEQ is taken.
        let clk0 = core.clk;

        step(&mut core, &mut bus, &mut int);

        assert_eq!(core.reg_pc, 0x1004, "BEQ +2 -> PC = $1000+2+2");
        assert_eq!(core.clk - clk0, 3, "taken same-page branch = 3 cycles");
        // A dummy read of the branch's reg_pc ($1002) occurred.
        assert!(
            bus.trace.iter().any(|(k, a, _)| *k == 'r' && *a == 0x1002),
            "taken branch emitted the reg_pc dummy read"
        );
        // DELAYS_INTERRUPT set (no page cross).
        assert!(
            opinfo_delays_interrupt(core.last_opcode_info) != 0,
            "same-page taken branch set DELAYS_INTERRUPT"
        );
    }
}
