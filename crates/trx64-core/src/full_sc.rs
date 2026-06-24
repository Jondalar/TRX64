//! full_sc.rs — wires the C64 to its dedicated VERBATIM x64sc 6510 SC core.
//!
//! This is the production C64 CPU path (replacing the shared microcode-pattern
//! `cpu.rs` / `Cpu6510` for the full machine). The verbatim core lives in
//! `c64_6510core.rs` (a 1:1 port of VICE's x64sc `6510dtvcore.c` via
//! `c64cpusc.c` / `mainc64cpu.c`); this module implements its host-hook surface
//! [`C64Core6510Bus`] over the existing assembled bus ([`crate::full::FullBus`])
//! and drives `c64_6510core_execute` once per instruction.
//!
//! WHY: the SC core threads `vic_cycle()` (the per-CLK Phi1/Phi2 VIC tick) +
//! `check_ba()` (the VIC BA cycle-steal) + the interrupt-delay counters into
//! EVERY bus access — the cycle-exact coupling the old pattern engine could not
//! reproduce. The interrupt model is the real OPINFO + delay-cycle-counter
//! `IntStatus` woven into DO_INTERRUPT, not the pattern engine's opcode-value
//! approximation.
//!
//! Trace parity: the verbatim core's bus trait is observer-agnostic
//! (pure `read_raw`/`write_raw`). We emit the [`Observer`] events from inside the
//! bus methods (on_bus per real access) and from the run loop (on_instruction per
//! retire), stamped with the live `reg_pc` + `clk` the core threads — reproducing
//! the exact `.c64retrace` record stream `cpu.rs` produced (the conformance
//! oracle, = VICE).

use crate::c64_6510core::{
    c64_6510core_execute, C64Core6510, C64Core6510Bus, IntStatus, OPERAND_BYTES,
};
use crate::cia::CIAT_TABLEN;
use crate::full::FullBus;
use crate::{BusKind, Observer};

/// The SC bus: the C64 verbatim core's host-hook surface. Wraps the assembled
/// [`FullBus`] (reusing its EXACT PLA-banked read/write dispatch, the $DD00 IEC
/// push-flush, the keyboard, the side-effect queues) and adds:
///   * the [`Observer`] emit (on_bus per real Read/Write access),
///   * the live `reg_pc`/`clk` threading (read from the core via raw pointers so
///     each bus record is stamped exactly as `cpu.rs` stamped it),
///   * the verbatim-core cycle hooks (`vic_cycle` / `check_ba` / `process_alarms`).
pub struct FullScBus<'a, 'o, O: Observer> {
    /// The assembled bus — all banking / IO / IEC / keyboard dispatch is reused
    /// verbatim from here.
    pub fb: FullBus<'a>,
    /// Trace observer. on_bus is emitted from `read_raw`/`write_raw`.
    pub obs: &'o mut O,
    /// Live `reg_pc` of the executing core (= the `pc` field of every bus record;
    /// `cpu.rs` passed `self.reg_pc`). Read-only raw pointer; the core invokes the
    /// bus synchronously and never holds a live `&mut` to `reg_pc` at the instant a
    /// bus method reads it (the same disjoint-field pattern the drive's `clk_ptr`
    /// uses). Set per instruction by the run loop.
    pub core_pc: *const u16,
    /// Live `clk` of the executing core (= the `cycle` field of every bus record;
    /// `cpu.rs` passed `self.clk` BEFORE the cycle's tick — the verbatim core's
    /// `read_raw`/`write_raw` likewise run BEFORE the matching `clk_inc`, so this
    /// raw read yields the same pre-increment clk). See `core_pc` for the safety
    /// argument.
    pub core_clk: *const u64,
    /// Fetch-capture from `debug_maincpu` (called by the core right after the
    /// opcode + operands are fetched, BEFORE the opcode body runs). The run loop
    /// reads it back to emit the `on_instruction` CPU_STEP record with the right
    /// opcode-PC + operand bytes. `(opcode_pc, opcode, p1, p2hi, fetch_clk)`.
    pub fetch: Option<(u16, u8, u8, u8, u64)>,
    /// The opcode-PC + opcode of the instruction currently executing (latched by
    /// `debug_maincpu` at fetch). Used to compute the bus-record `pc` field to the
    /// TS/cpu.rs convention (= reg_pc AFTER the operand fetch), since the verbatim
    /// core advances `reg_pc` only in the opcode body's `inc_pc` — which runs AFTER
    /// the data read for read-ops, leaving the live `reg_pc` stale at the access.
    /// `(opcode_pc, opcode)`.
    pub cur_op: (u16, u8),
    /// True once `debug_maincpu` has fired for THIS execute call's opcode (i.e. the
    /// opcode body is running). Bus accesses BEFORE this (the prologue's pending
    /// IRQ/NMI/RESET dispatch — the dummy reads, stack pushes of PCH/PCL/P, and the
    /// vector reads) use the LIVE reg_pc directly (= cpu.rs `service_interrupt`,
    /// which pushed `self.reg_pc` — the return address). After the fetch, the body
    /// accesses use the post-fetch synthetic pc (`opcode_pc + 1 + OPERAND_BYTES`).
    pub fetched: bool,
}

impl<'a, 'o, O: Observer> FullScBus<'a, 'o, O> {
    /// The bus-record `pc` field, to the TS/cpu.rs convention: the reg_pc value
    /// cpu.rs had at the access (= opcode_pc + bytes consumed from the instruction
    /// stream). cpu.rs advanced reg_pc DURING the operand fetch, so by the body's
    /// data accesses reg_pc = opcode_pc + 1 + OPERAND_BYTES. The verbatim core
    /// advances reg_pc only in the opcode body's `inc_pc`, which for read-ops runs
    /// AFTER the data read — so the live `reg_pc` is still the opcode address there.
    ///
    /// Rule: if the live `reg_pc` has NOT yet advanced past the opcode address
    /// (`live == opcode_pc`, i.e. `inc_pc` hasn't run), use the synthetic
    /// post-fetch value `opcode_pc + 1 + OPERAND_BYTES[op]`. Otherwise the live
    /// reg_pc is already the correct post-advance value (e.g. JSR's pushes happen
    /// after `inc_pc(2)` → reg_pc = opcode_pc+2 = only-lo-consumed, the exact TS
    /// value; STA's write happens after `inc_pc` → reg_pc already correct), so use
    /// it directly.
    #[inline]
    fn pc(&self) -> u16 {
        // SAFETY: `core_pc` points at `C64Core6510.reg_pc`, disjoint from every
        // field `fb`/`obs` own; read synchronously inside a bus call the core
        // itself invoked; single-threaded; never aliased by a live `&mut` to that
        // same u16 at the instant of the read.
        let live = unsafe { *self.core_pc };
        // Pre-fetch phase (the prologue's interrupt/reset dispatch): the live
        // reg_pc is the value cpu.rs pushed/read with (the return address), so use
        // it directly. The opcode-body synthetic only applies once the instruction's
        // opcode has been fetched.
        if !self.fetched {
            return live;
        }
        let (opcode_pc, opcode) = self.cur_op;
        if live == opcode_pc {
            opcode_pc.wrapping_add(1 + OPERAND_BYTES[opcode as usize] as u16)
        } else {
            live
        }
    }
    /// Live executing-core `clk` (the bus-record `cycle`).
    #[inline]
    fn clk(&self) -> u64 {
        // SAFETY: same disjoint-field reasoning as `pc`.
        unsafe { *self.core_clk }
    }

    /// Keep the wrapped FullBus's master clock equal to the core's live clk. The
    /// verbatim core owns the authoritative clock (`C64Core6510.clk`); the CIAs
    /// and the IEC push-flush read the FullBus `clk`, so it must track the core's
    /// at every access instant (= VICE `maincpu_clk` shared by the alarm/IEC code).
    #[inline]
    fn sync_clk(&mut self) {
        let c = self.clk();
        self.fb.clk = c;
        self.fb.cia1.clk = c;
        self.fb.cia2.clk = c;
    }
}

impl<'a, 'o, O: Observer> C64Core6510Bus for FullScBus<'a, 'o, O> {
    /// LOAD path (mainc64cpu.c:359-363) real read. Reuses [`FullBus`]'s banked
    /// read dispatch EXACTLY, then emits the `on_bus(Read)` record (+ any chip
    /// side-effect reads, e.g. the $DD00 IEC `iecReadPins` indirection, emitted
    /// BEFORE this load's own record — matching the TS `emitC64Access`-then-read
    /// order `cpu.rs` reproduced).
    #[inline]
    fn read_raw(&mut self, addr: u16) -> u8 {
        self.sync_clk();
        let v = crate::cpu::Bus::read(&mut self.fb, addr);
        let pc = self.pc();
        let clk = self.clk();
        let mut se: Vec<(u16, u8)> = Vec::new();
        crate::cpu::Bus::take_side_effect_reads(&mut self.fb, &mut se);
        for (a, val) in se {
            self.obs.on_bus(BusKind::Read, a, val, pc, clk, 0);
        }
        self.obs.on_bus(BusKind::Read, addr, v, pc, clk, 0);
        v
    }

    /// STORE path (mainc64cpu.c:372-379) real write. Reuses [`FullBus`]'s banked
    /// write dispatch EXACTLY, then emits the `on_bus(Write)` record (+ any chip
    /// side-effect writes, e.g. the CIA2 PA → $DD00 IEC re-push, emitted BEFORE the
    /// originating store's own record). The pre-write `old` byte is captured ONLY
    /// for the side-effect-free RAM window ($0002..$D000) — the trace carries
    /// `hasOld` only there (Spec 753); for the IO window we skip the pre-read
    /// entirely (avoiding a spurious chip-register read side effect) since the
    /// observer discards `old` for $D000-$DFFF.
    #[inline]
    fn write_raw(&mut self, addr: u16, value: u8) {
        self.sync_clk();
        let old = if (0x0002..0xd000).contains(&addr) {
            crate::cpu::Bus::read(&mut self.fb, addr)
        } else {
            0
        };
        crate::cpu::Bus::write(&mut self.fb, addr, value);
        let pc = self.pc();
        let clk = self.clk();
        let mut se: Vec<(u16, u8, u8)> = Vec::new();
        crate::cpu::Bus::take_side_effect_writes(&mut self.fb, &mut se);
        for (a, v, o) in se {
            self.obs.on_bus(BusKind::Write, a, v, pc, clk, o);
        }
        self.obs.on_bus(BusKind::Write, addr, value, pc, clk, old);
    }

    /// FETCH read (FETCH_OPCODE) — the opcode/operand byte fetch. Reuses the banked
    /// read dispatch, then emits a `BusKind::Fetch` record (FILTERED OUT of the
    /// trace by the observer, exactly as `cpu.rs::load_fetch` was). We do NOT drain
    /// side-effect reads here (a fetch is never a $DD00 PA sample).
    #[inline]
    fn read_raw_fetch(&mut self, addr: u16) -> u8 {
        self.sync_clk();
        let v = crate::cpu::Bus::read(&mut self.fb, addr);
        let pc = self.pc();
        let clk = self.clk();
        self.obs.on_bus(BusKind::Fetch, addr, v, pc, clk, 0);
        v
    }

    /// DUMMY read (mainc64cpu.c:365-369 minus check_ba). The bus side effect of a
    /// dummy read is real (VICE reads the actual address), but the observer FILTERS
    /// DummyRead out of the trace, so we do the banked read for its side effects and
    /// emit a DummyRead record (discarded). We must NOT drain side-effect reads here
    /// (a dummy read of $DD00 is not a real PA sample in the trace contract — and
    /// `cpu.rs::load_dummy` likewise never drained them).
    #[inline]
    fn read_raw_dummy(&mut self, addr: u16) -> u8 {
        self.sync_clk();
        let v = crate::cpu::Bus::read(&mut self.fb, addr);
        let pc = self.pc();
        let clk = self.clk();
        self.obs.on_bus(BusKind::DummyRead, addr, v, pc, clk, 0);
        v
    }

    /// DUMMY write (mainc64cpu.c:382-388 minus the $ff00 reu hook). The RMW dummy
    /// write-back: VICE writes the OLD value, then the real STORE writes the new.
    /// For RAM that nets to the new value; for IO the shadow takes the old then new.
    /// DummyWrite is filtered out of the trace by the observer.
    #[inline]
    fn write_raw_dummy(&mut self, addr: u16, value: u8) {
        self.sync_clk();
        let old = if (0x0002..0xd000).contains(&addr) {
            crate::cpu::Bus::read(&mut self.fb, addr)
        } else {
            0
        };
        crate::cpu::Bus::write(&mut self.fb, addr, value);
        let pc = self.pc();
        let clk = self.clk();
        self.obs.on_bus(BusKind::DummyWrite, addr, value, pc, clk, old);
    }

    /// check_ba (mainc64cpu.c:194-208) — the VIC BA cycle-steal. Reuses the
    /// FullBus's `check_ba_before_read` (= `vicii_steal_cycles`, which ticks the
    /// VIC + advances the shared clk + CIAs per stolen cycle). Returns the stolen
    /// count; the SC core's Exec adds it to `core.clk`. The `check_ba_low` /
    /// last_opcode_info ENABLES_IRQ steal-signal for SH*/CLI is not surfaced by the
    /// stock-machine VIC steal (no DMA controller wired), so we leave `loi`
    /// untouched — matching the old `Cpu6510` path, which also never set it.
    #[inline]
    fn check_ba(&mut self, _last_opcode_info: &mut u32, _check_ba_low: bool) -> u64 {
        self.sync_clk();
        crate::cpu::Bus::check_ba_before_read(&mut self.fb) as u64
    }

    /// CLK_INC's per-cycle VIC tick (c64cpusc.c:47-51). The core has already
    /// incremented `core.clk` to `clk` and now ticks the VIC one cycle. We advance
    /// the VIC + both CIAs to `clk` (= `FullBus::tick` semantics, but pinned to the
    /// core's authoritative clk rather than self-incrementing the FullBus clock).
    #[inline]
    fn vic_cycle(&mut self, clk: u64) {
        let vbank = self.fb.vic_bank_base();
        let view = crate::vic::VicMemView {
            ram: self.fb.ram,
            char_rom: Some(self.fb.char_rom),
            color_ram: &self.fb.io[0x0800..0x0c00],
            vbank,
        };
        self.fb.vic.tick(&view);
        self.fb.clk = clk;
        self.fb.cia1.clk = clk;
        self.fb.cia2.clk = clk;
        self.fb.cia1.tick(self.fb.cia_table);
        self.fb.cia2.tick(self.fb.cia_table);
    }

    /// PROCESS_ALARMS — advance the CIA timer state machines up to `clk` so any
    /// underflow latches its ICR flag at the exact cycle (the interrupt-line
    /// refresh in the run loop then samples them into IntStatus). The VIC raster
    /// machinery already advanced via `vic_cycle`.
    #[inline]
    fn process_alarms(&mut self, clk: u64) {
        let table: &[u16; CIAT_TABLEN] = self.fb.cia_table;
        self.fb.cia1.update_to(clk, table);
        self.fb.cia2.update_to(clk, table);
    }

    /// cpu_reset (mainc64cpu.c:631-651) — invoked on the IK_RESET dispatch. The
    /// C64 reset vector ($FFFC/$FFFD) is read by DO_INTERRUPT itself (through
    /// `load`); there is no extra state to seed here for the stock machine (the
    /// power-on clk offset is set by the Machine's cold_reset). No-op.
    #[inline]
    fn cpu_reset(&mut self) {}

    /// Tracing hook (dtv:1822-1833) — called right after the opcode + operands are
    /// fetched (reg_pc still at the opcode address), BEFORE the opcode body runs.
    /// We capture the opcode-PC + opcode + operand bytes so the run loop can emit
    /// the `on_instruction` CPU_STEP record with the right fields (the verbatim
    /// core has no post-instruction register hook; the run loop reads the live regs
    /// after `execute` returns and pairs them with this fetch capture).
    #[inline]
    fn debug_maincpu(&mut self, pc: u16, clk: u64, op: u8, p1: u8, p2hi: u8) {
        self.fetch = Some((pc, op, p1, p2hi, clk));
        self.cur_op = (pc, op);
        self.fetched = true;
    }

    /// Interrupt-serviced hook — forwards the verbatim core's DO_INTERRUPT
    /// vector-selection event into the [`Observer`]. This is the ONLY producer of
    /// `Observer::on_interrupt` on the full SC path (the CPU-isolated `cpu.rs`
    /// path fires it directly from `service_interrupt`). Stamped with the
    /// (vector, clk) the core chose (= cpu65xx-vice.ts onInterruptServiced).
    #[inline]
    fn on_interrupt(&mut self, vector: u16, clk: u64) {
        self.obs.on_interrupt(vector, clk);
    }
}

/// Drive the C64 verbatim SC core for one whole instruction (plus any pending
/// interrupt / reset dispatch that runs first in the prologue), emitting the
/// `on_instruction` retire record for the executed opcode. Returns the JAM
/// disposition from [`c64_6510core_execute`].
///
/// `on_instruction` parity (= `cpu.rs`): the CPU_STEP record carries the
/// POST-instruction registers, the opcode-PC, the opcode, and the two raw operand
/// bytes, stamped at the clk `cpu.rs` used — which is the post-instruction clk
/// MINUS 1 (cpu.rs emitted `self.clk` BEFORE the retiring cycle's trailing tick;
/// the verbatim core's final `clk_inc` has already run by the time `execute`
/// returns, so we subtract that one trailing cycle). The `(opcode_pc, opcode, p1,
/// p2hi)` come from the `debug_maincpu` fetch capture; `p2hi` is the operand high
/// byte for 3-byte opcodes (0 for 1/2-byte, matching `cpu.rs`'s `b2`).
pub fn execute_one<O: Observer>(
    core: &mut C64Core6510,
    bus: &mut FullScBus<O>,
    int: &mut IntStatus,
) -> i32 {
    bus.fetch = None;
    let result = c64_6510core_execute(core, bus, int);
    if let Some((opcode_pc, opcode, _p1, _p2hi, _fetch_clk)) = bus.fetch.take() {
        // POST-instruction clk minus the one trailing clk_inc, reproducing
        // cpu.rs's pre-final-tick stamp (the CPU_STEP `cycle` field).
        let clk = core.clk.wrapping_sub(1);
        // The CPU_STEP record carries b1=operand-lo, b2=operand-hi, read from memory
        // at opcode_pc+1 / opcode_pc+2 (= cpu.rs `continue_instruction_cycle`'s
        // `bus.read(prev_pc+1/+2)` at the retire boundary, with the post-instruction
        // banking) — and 0 for the bytes the opcode does not have. JSR ($20) is a
        // 3-byte opcode whose high byte is fetched late (not in FETCH_OPCODE), so a
        // re-read from memory is the only source that is always correct.
        let nbytes = OPERAND_BYTES[opcode as usize];
        let b1 = if nbytes >= 1 {
            crate::cpu::Bus::read(&mut bus.fb, opcode_pc.wrapping_add(1))
        } else {
            0
        };
        let b2 = if nbytes >= 2 {
            crate::cpu::Bus::read(&mut bus.fb, opcode_pc.wrapping_add(2))
        } else {
            0
        };
        // `cpu.rs` emitted the RAW `reg_p` (with the N/Z shadows masked OUT — the
        // TS oracle's CPU_STEP `p` field carries only the non-N/Z flags + UNUSED,
        // NOT the composite status). The verbatim core's `reg_p` is likewise
        // P with P_ZERO|P_SIGN masked out, so we pass it directly.
        bus.obs.on_instruction(
            opcode_pc,
            opcode,
            b1,
            b2,
            core.reg_a,
            core.reg_x,
            core.reg_y,
            core.reg_sp,
            core.reg_p,
            clk,
        );
    }
    result
}
