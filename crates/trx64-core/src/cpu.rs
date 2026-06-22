//! cpu.rs — cycle-stepped 6510, 1:1 port of cpu/cpu65xx-vice.ts.
//!
//! The CPU is the clock master: [`Cpu6510::execute_cycle`] advances exactly one
//! master cycle. A microcode engine (tables.rs) drives addressing modes via
//! per-pattern op-string lists, interpreted identically to the TS
//! `executeMicroOp` switch. ALU, flags, illegals and the 7-cycle interrupt
//! entry mirror the TS source op-for-op.
//!
//! CPU-ISOLATED scope (Phase-1 gate): the bus is a flat 64K RAM array (no VIC /
//! CIA / banking / $00-$01 port hook), there is no alarm context, and no chip
//! asserts IRQ/NMI. The interrupt-dispatch structure is ported and present but
//! inert under these conditions — deterministic, exactly as the spec demands
//! for verifying the CPU in isolation. VIC/CIA coupling lands in later builders.

use crate::tables::{addr_mode_pattern, MICROCODE_TABLE, UNDOC_TABLE, MicroEntry};
use crate::{BusKind, Observer};

// VICE flag bits (src/6510core.c P_* enumeration).
const P_SIGN: u8 = 0x80;
const P_OVERFLOW: u8 = 0x40;
const P_UNUSED: u8 = 0x20;
const P_DECIMAL: u8 = 0x08;
const P_INTERRUPT: u8 = 0x04;
const P_ZERO: u8 = 0x02;
const P_CARRY: u8 = 0x01;

/// Bus a CPU core reads/writes through (= TS `CpuMemory`). For the isolated gate
/// this is a flat 64K RAM; later a banked bus implements the same trait.
pub trait Bus {
    fn read(&mut self, addr: u16) -> u8;
    fn write(&mut self, addr: u16, value: u8);
}

/// In-flight instruction microcode state (= TS `InstructionState`).
#[derive(Clone)]
struct Inst {
    entry: MicroEntry,
    micro_idx: usize,
    microcode: &'static [&'static str],
    operand_lo: u16,
    operand_hi: u16,
    ea: u16,
    ind_ptr: u16,
    fetched_value: u8,
    opcode_pc: u16,
    opcode_byte: u8,
}

/// Cycle-stepped 6510 core. Field names follow the VICE-derived TS port.
#[derive(Clone)]
pub struct Cpu6510 {
    pub reg_pc: u16,
    pub reg_a: u8,
    pub reg_x: u8,
    pub reg_y: u8,
    pub reg_sp: u8,
    pub reg_p: u8,
    /// 0x80 if N set, else 0 (= VICE flag_n cache).
    pub flag_n: u8,
    /// 0 if Z set; non-zero if Z clear (= VICE flag_z cache).
    pub flag_z: u8,
    /// Monotonic master clock (never wraps — Spec 743).
    pub clk: u64,

    jammed: bool,
    at_boundary: bool,
    inst: Option<Inst>,
    interrupt_dispatched_this_cycle: bool,
}

impl Default for Cpu6510 {
    fn default() -> Self {
        Self {
            reg_pc: 0,
            reg_a: 0,
            reg_x: 0,
            reg_y: 0,
            reg_sp: 0xff,
            reg_p: P_UNUSED,
            flag_n: 0,
            flag_z: 1,
            clk: 0,
            jammed: false,
            at_boundary: true,
            inst: None,
            interrupt_dispatched_this_cycle: false,
        }
    }
}

impl Cpu6510 {
    pub fn new() -> Self {
        Self::default()
    }

    /// Composite P register incl. flag_n/flag_z view (= TS `flags` getter).
    #[inline]
    pub fn flags(&self) -> u8 {
        (self.reg_p & !(P_SIGN | P_ZERO))
            | (self.flag_n & P_SIGN)
            | (if self.flag_z == 0 { P_ZERO } else { 0 })
            | P_UNUSED
    }

    #[inline]
    fn set_flags(&mut self, v: u8) {
        self.reg_p = v & !(P_SIGN | P_ZERO);
        self.flag_n = v & P_SIGN;
        self.flag_z = if v & P_ZERO != 0 { 0 } else { 1 };
    }

    pub fn is_at_boundary(&self) -> bool {
        self.at_boundary
    }
    pub fn is_jammed(&self) -> bool {
        self.jammed
    }

    // -------- bus primitives --------
    #[inline]
    fn load<B: Bus, O: Observer>(&mut self, bus: &mut B, _obs: &mut O, addr: u16) -> u8 {
        bus.read(addr)
    }

    #[inline]
    fn load_fetch<B: Bus, O: Observer>(&mut self, bus: &mut B, obs: &mut O, addr: u16) -> u8 {
        let v = self.load(bus, obs, addr);
        obs.on_bus(BusKind::Fetch, addr, v);
        v
    }

    #[inline]
    fn load_read<B: Bus, O: Observer>(&mut self, bus: &mut B, obs: &mut O, addr: u16) -> u8 {
        let v = self.load(bus, obs, addr);
        obs.on_bus(BusKind::Read, addr, v);
        v
    }

    #[inline]
    fn load_dummy<B: Bus, O: Observer>(&mut self, bus: &mut B, obs: &mut O, addr: u16) -> u8 {
        let v = self.load(bus, obs, addr);
        obs.on_bus(BusKind::DummyRead, addr, v);
        v
    }

    #[inline]
    fn store<B: Bus, O: Observer>(&mut self, bus: &mut B, obs: &mut O, addr: u16, value: u8) {
        bus.write(addr, value);
        obs.on_bus(BusKind::Write, addr, value);
    }

    #[inline]
    fn store_dummy<B: Bus, O: Observer>(&mut self, bus: &mut B, obs: &mut O, addr: u16, value: u8) {
        bus.write(addr, value);
        obs.on_bus(BusKind::DummyWrite, addr, value);
    }

    fn push_byte<B: Bus, O: Observer>(&mut self, bus: &mut B, obs: &mut O, v: u8) {
        self.store(bus, obs, 0x0100 + self.reg_sp as u16, v);
        self.reg_sp = self.reg_sp.wrapping_sub(1);
    }

    fn pop_byte<B: Bus, O: Observer>(&mut self, bus: &mut B, obs: &mut O) -> u8 {
        self.reg_sp = self.reg_sp.wrapping_add(1);
        self.load(bus, obs, 0x0100 + self.reg_sp as u16)
    }

    /// Reset PC from supplied vector (isolated gate sets PC directly; ROM reset
    /// path reads $FFFC/$FFFD — handled by the caller via `set_pc`).
    pub fn reset_to(&mut self, pc: u16) {
        self.reg_a = 0;
        self.reg_x = 0;
        self.reg_y = 0;
        self.reg_sp = 0xff;
        self.reg_p = P_UNUSED;
        self.flag_n = 0;
        self.flag_z = 1;
        self.clk = 0;
        self.at_boundary = true;
        self.inst = None;
        self.jammed = false;
        self.reg_pc = pc;
    }

    // -------- CLK_INC tick (c64cpusc.c:47) --------
    // Isolated gate: no alarm context, no VIC hook. tick() is just clk++.
    #[inline]
    fn tick(&mut self) {
        self.clk = self.clk.wrapping_add(1);
    }

    // -------- per-cycle entry (= executeCycle) --------
    pub fn execute_cycle<B: Bus, O: Observer>(&mut self, bus: &mut B, obs: &mut O) {
        if self.jammed {
            self.tick();
            return;
        }
        self.interrupt_dispatched_this_cycle = false;
        if self.at_boundary {
            self.start_instruction_cycle(bus, obs);
        } else {
            self.continue_instruction_cycle(bus, obs);
        }
        if !self.interrupt_dispatched_this_cycle {
            self.tick();
        }
    }

    fn start_instruction_cycle<B: Bus, O: Observer>(&mut self, bus: &mut B, obs: &mut O) {
        // Isolated gate: no NMI/IRQ source asserts, so DO_INTERRUPT is never
        // entered. Opcode fetch only.
        let pc_fetch = self.reg_pc;
        let opcode = self.load_fetch(bus, obs, pc_fetch);
        self.reg_pc = self.reg_pc.wrapping_add(1);

        let entry = MICROCODE_TABLE[opcode as usize];
        match entry {
            None => {
                self.execute_illegal_opcode(bus, obs, opcode, pc_fetch);
                // TS illegal path passes b1=0, b2=0 on this first emit.
                obs.on_instruction(
                    pc_fetch, opcode, 0, 0, self.reg_a, self.reg_x, self.reg_y, self.reg_sp,
                    self.flags(), self.clk,
                );
            }
            Some(entry) => {
                let microcode = addr_mode_pattern(entry.pattern);
                if microcode.len() <= 1 {
                    let mut fs = self.make_fresh_state(entry, microcode, pc_fetch, opcode);
                    self.execute_final_op(bus, obs, &mut fs);
                    obs.on_instruction(
                        pc_fetch, opcode, (fs.operand_lo & 0xff) as u8, (fs.operand_hi & 0xff) as u8,
                        self.reg_a, self.reg_x, self.reg_y, self.reg_sp, self.flags(), self.clk,
                    );
                    return;
                }
                let mut inst = self.make_fresh_state(entry, microcode, pc_fetch, opcode);
                inst.micro_idx = 1;
                self.inst = Some(inst);
                self.at_boundary = false;
            }
        }
    }

    fn continue_instruction_cycle<B: Bus, O: Observer>(&mut self, bus: &mut B, obs: &mut O) {
        let mut inst = self.inst.take().expect("continue without inst");
        let op = inst.microcode[inst.micro_idx];
        inst.micro_idx += 1;
        let is_final = inst.micro_idx >= inst.microcode.len();
        self.execute_micro_op(bus, obs, op, &mut inst);
        if is_final {
            let prev_pc = inst.opcode_pc;
            let opcode_byte = inst.opcode_byte;
            // Spec 723: raw operand bytes peeked from memory at prevPc+1/+2.
            let op_len = operand_byte_count(inst.entry.mode);
            let b1 = if op_len >= 1 { bus.read(prev_pc.wrapping_add(1)) } else { 0 };
            let b2 = if op_len >= 2 { bus.read(prev_pc.wrapping_add(2)) } else { 0 };
            self.execute_final_op(bus, obs, &mut inst);
            self.at_boundary = true;
            self.inst = None;
            obs.on_instruction(
                prev_pc, opcode_byte, b1, b2, self.reg_a, self.reg_x, self.reg_y, self.reg_sp,
                self.flags(), self.clk,
            );
        } else {
            self.inst = Some(inst);
        }
    }

    fn make_fresh_state(
        &self,
        entry: MicroEntry,
        microcode: &'static [&'static str],
        pc_fetch: u16,
        opcode_byte: u8,
    ) -> Inst {
        Inst {
            entry,
            micro_idx: 0,
            microcode,
            operand_lo: 0,
            operand_hi: 0,
            ea: 0,
            ind_ptr: 0,
            fetched_value: 0,
            opcode_pc: pc_fetch,
            opcode_byte,
        }
    }

    // -------- micro-op dispatch (= executeMicroOp) --------
    fn execute_micro_op<B: Bus, O: Observer>(
        &mut self,
        bus: &mut B,
        obs: &mut O,
        op: &str,
        s: &mut Inst,
    ) {
        match op {
            "fetch_opcode" => {}
            "fetch_imm" => {
                s.operand_lo = self.load_fetch(bus, obs, self.reg_pc) as u16;
                self.reg_pc = self.reg_pc.wrapping_add(1);
            }
            "fetch_lo" => {
                s.operand_lo = self.load_fetch(bus, obs, self.reg_pc) as u16;
                self.reg_pc = self.reg_pc.wrapping_add(1);
            }
            "fetch_zp_lo" => {
                s.operand_lo = self.load_fetch(bus, obs, self.reg_pc) as u16;
                self.reg_pc = self.reg_pc.wrapping_add(1);
                s.ea = s.operand_lo & 0xff;
                s.ind_ptr = s.operand_lo & 0xff;
            }
            "fetch_hi" => {
                s.operand_hi = self.load_fetch(bus, obs, self.reg_pc) as u16;
                self.reg_pc = self.reg_pc.wrapping_add(1);
                s.ea = s.operand_lo | (s.operand_hi << 8);
            }
            "dummy_zp" => {
                self.load_dummy(bus, obs, s.ea);
                match s.entry.mode {
                    "zpx" => s.ea = (s.ea + self.reg_x as u16) & 0xff,
                    "zpy" => s.ea = (s.ea + self.reg_y as u16) & 0xff,
                    "indx" => s.ind_ptr = (s.ea + self.reg_x as u16) & 0xff,
                    _ => {}
                }
            }
            "fetch_ind_lo" => {
                s.operand_lo = self.load_read(bus, obs, s.ind_ptr) as u16;
            }
            "fetch_ind_hi" => {
                s.operand_hi = self.load_read(bus, obs, (s.ind_ptr + 1) & 0xff) as u16;
                s.ea = s.operand_lo | (s.operand_hi << 8);
            }
            "dummy_addr" => {
                let base = s.operand_lo | (s.operand_hi << 8);
                let idx = if s.entry.mode == "absx" { self.reg_x } else { self.reg_y } as u16;
                let ea_candidate = base.wrapping_add(idx);
                self.load_dummy(bus, obs, (base & 0xff00) | (ea_candidate & 0xff));
                s.ea = ea_candidate;
            }
            "read_ea" => {
                s.fetched_value = self.load_read(bus, obs, s.ea);
            }
            "write_ea" => {
                self.execute_store(bus, obs, s);
            }
            "dummy_write_ea_old" => {
                self.store_dummy(bus, obs, s.ea, s.fetched_value);
            }
            "write_ea_new" => {
                let r = self.compute_rmw_result(s);
                self.store(bus, obs, s.ea, r);
            }
            "read_ea_pgx" => {
                let base = s.operand_lo | (s.operand_hi << 8);
                let ea = base.wrapping_add(self.reg_x as u16);
                s.ea = ea;
                if (base & 0xff00) != (ea & 0xff00) {
                    self.load_dummy(bus, obs, (base & 0xff00) | (ea & 0xff));
                    self.tick();
                }
                s.fetched_value = self.load_read(bus, obs, ea);
            }
            "read_ea_pgy" => {
                let base = s.operand_lo | (s.operand_hi << 8);
                let ea = base.wrapping_add(self.reg_y as u16);
                s.ea = ea;
                if (base & 0xff00) != (ea & 0xff00) {
                    self.load_dummy(bus, obs, (base & 0xff00) | (ea & 0xff));
                    self.tick();
                }
                s.fetched_value = self.load_read(bus, obs, ea);
            }
            "read_ea_lo" => {
                s.operand_lo = self.load_read(bus, obs, s.ea) as u16;
            }
            "read_ea_hi" => {
                s.operand_hi =
                    self.load_read(bus, obs, (s.ea & 0xff00) | ((s.ea + 1) & 0xff)) as u16;
                self.reg_pc = s.operand_lo | (s.operand_hi << 8);
            }
            "internal" => {}
            "push" => {
                let v = if s.entry.op == "pha" { self.reg_a } else { self.flags() | 0x10 };
                self.push_byte(bus, obs, v);
            }
            "pop" => {
                s.fetched_value = self.pop_byte(bus, obs);
            }
            "dummy_sp" => {
                self.load_dummy(bus, obs, 0x0100 + self.reg_sp as u16);
            }
            "push_pch" => {
                self.push_byte(bus, obs, (self.reg_pc >> 8) as u8);
            }
            "push_pcl" => {
                self.push_byte(bus, obs, (self.reg_pc & 0xff) as u8);
            }
            "push_p_brk" => {
                self.push_byte(bus, obs, self.flags() | 0x10);
                self.reg_p |= P_INTERRUPT;
            }
            "pop_p" => {
                let v = self.pop_byte(bus, obs);
                self.set_flags(v & !0x10);
            }
            "pop_pcl" => {
                s.operand_lo = self.pop_byte(bus, obs) as u16;
            }
            "pop_pch" => {
                s.operand_hi = self.pop_byte(bus, obs) as u16;
                self.reg_pc = s.operand_lo | (s.operand_hi << 8);
            }
            "fetch_pc_dummy" => {
                self.load_dummy(bus, obs, self.reg_pc);
            }
            "read_brk_vec_lo" => {
                s.operand_lo = self.load_read(bus, obs, 0xfffe) as u16;
            }
            "read_brk_vec_hi" => {
                s.operand_hi = self.load_read(bus, obs, 0xffff) as u16;
                self.reg_pc = s.operand_lo | (s.operand_hi << 8);
            }
            "fetch_dummy_pc" => {
                self.load_dummy(bus, obs, self.reg_pc);
                self.reg_pc = self.reg_pc.wrapping_add(1);
            }
            _ => {}
        }
    }

    // -------- final op dispatch (= executeFinalOp) --------
    fn execute_final_op<B: Bus, O: Observer>(&mut self, bus: &mut B, obs: &mut O, s: &mut Inst) {
        let op = s.entry.op;
        let mode = s.entry.mode;
        let value_in = if mode == "imm" || mode == "rel" {
            (s.operand_lo & 0xff) as u8
        } else {
            s.fetched_value
        };
        match op {
            "lda" => { self.reg_a = value_in; self.update_nz(self.reg_a); }
            "ldx" => { self.reg_x = value_in; self.update_nz(self.reg_x); }
            "ldy" => { self.reg_y = value_in; self.update_nz(self.reg_y); }
            "sta" | "stx" | "sty" => {}
            "and" => { self.reg_a &= value_in; self.update_nz(self.reg_a); }
            "ora" => { self.reg_a |= value_in; self.update_nz(self.reg_a); }
            "eor" => { self.reg_a ^= value_in; self.update_nz(self.reg_a); }
            "adc" => self.adc(value_in),
            "sbc" => self.sbc(value_in),
            "cmp" => self.compare(self.reg_a, value_in),
            "cpx" => self.compare(self.reg_x, value_in),
            "cpy" => self.compare(self.reg_y, value_in),
            "bit" => self.bit_op(value_in),
            "inc" | "dec" | "asl" | "lsr" | "rol" | "ror" => {
                if mode == "acc" {
                    let r = self.compute_rmw_on_value(op, self.reg_a);
                    self.reg_a = r;
                    self.update_nz(self.reg_a);
                }
            }
            "clc" => self.reg_p &= !P_CARRY,
            "sec" => self.reg_p |= P_CARRY,
            "cli" => self.reg_p &= !P_INTERRUPT,
            "sei" => self.reg_p |= P_INTERRUPT,
            "cld" => self.reg_p &= !P_DECIMAL,
            "sed" => self.reg_p |= P_DECIMAL,
            "clv" => self.reg_p &= !P_OVERFLOW,
            "tax" => { self.reg_x = self.reg_a; self.update_nz(self.reg_x); }
            "tay" => { self.reg_y = self.reg_a; self.update_nz(self.reg_y); }
            "tsx" => { self.reg_x = self.reg_sp; self.update_nz(self.reg_x); }
            "txa" => { self.reg_a = self.reg_x; self.update_nz(self.reg_a); }
            "txs" => self.reg_sp = self.reg_x,
            "tya" => { self.reg_a = self.reg_y; self.update_nz(self.reg_a); }
            "inx" => { self.reg_x = self.reg_x.wrapping_add(1); self.update_nz(self.reg_x); }
            "iny" => { self.reg_y = self.reg_y.wrapping_add(1); self.update_nz(self.reg_y); }
            "dex" => { self.reg_x = self.reg_x.wrapping_sub(1); self.update_nz(self.reg_x); }
            "dey" => { self.reg_y = self.reg_y.wrapping_sub(1); self.update_nz(self.reg_y); }
            "nop" => {}
            "pha" | "php" => {}
            "pla" => { self.reg_a = s.fetched_value; self.update_nz(self.reg_a); }
            "plp" => { self.set_flags(s.fetched_value & !0x10); }
            "bcc" => { if self.reg_p & P_CARRY == 0 { self.take_branch(obs, s.operand_lo as u8); } }
            "bcs" => { if self.reg_p & P_CARRY != 0 { self.take_branch(obs, s.operand_lo as u8); } }
            "bne" => { if self.flag_z != 0 { self.take_branch(obs, s.operand_lo as u8); } }
            "beq" => { if self.flag_z == 0 { self.take_branch(obs, s.operand_lo as u8); } }
            "bpl" => { if self.flag_n & 0x80 == 0 { self.take_branch(obs, s.operand_lo as u8); } }
            "bmi" => { if self.flag_n & 0x80 != 0 { self.take_branch(obs, s.operand_lo as u8); } }
            "bvc" => { if self.reg_p & P_OVERFLOW == 0 { self.take_branch(obs, s.operand_lo as u8); } }
            "bvs" => { if self.reg_p & P_OVERFLOW != 0 { self.take_branch(obs, s.operand_lo as u8); } }
            "jmp" => { if mode == "abs" { self.reg_pc = s.ea; } }
            "jsr" => { self.reg_pc = s.operand_lo | (s.operand_hi << 8); }
            "rts" => { self.reg_pc = self.reg_pc.wrapping_add(1); }
            "rti" => {}
            "brk" => {}
            _ => {}
        }
        let _ = bus;
    }

    fn compute_rmw_result(&mut self, s: &Inst) -> u8 {
        self.compute_rmw_on_value(s.entry.op, s.fetched_value)
    }

    fn compute_rmw_on_value(&mut self, op: &str, value: u8) -> u8 {
        let v = value;
        match op {
            "inc" => { let r = v.wrapping_add(1); self.update_nz(r); r }
            "dec" => { let r = v.wrapping_sub(1); self.update_nz(r); r }
            "asl" => {
                self.set_carry(v & 0x80 != 0);
                let r = v << 1;
                self.update_nz(r);
                r
            }
            "lsr" => {
                self.set_carry(v & 0x01 != 0);
                let r = v >> 1;
                self.update_nz(r);
                r
            }
            "rol" => {
                let old_c = self.reg_p & P_CARRY;
                self.set_carry(v & 0x80 != 0);
                let r = (v << 1) | if old_c != 0 { 1 } else { 0 };
                self.update_nz(r);
                r
            }
            "ror" => {
                let old_c = self.reg_p & P_CARRY;
                self.set_carry(v & 0x01 != 0);
                let r = (v >> 1) | if old_c != 0 { 0x80 } else { 0 };
                self.update_nz(r);
                r
            }
            _ => v,
        }
    }

    fn execute_store<B: Bus, O: Observer>(&mut self, bus: &mut B, obs: &mut O, s: &Inst) {
        let v = match s.entry.op {
            "sta" => self.reg_a,
            "stx" => self.reg_x,
            "sty" => self.reg_y,
            _ => return,
        };
        self.store(bus, obs, s.ea, v);
    }

    fn take_branch<O: Observer>(&mut self, _obs: &mut O, offset: u8) {
        let signed = offset as i8 as i16;
        let old_pc = self.reg_pc;
        self.reg_pc = (self.reg_pc as i16).wrapping_add(signed) as u16;
        self.tick(); // branch taken = +1 cycle
        if (old_pc & 0xff00) != (self.reg_pc & 0xff00) {
            self.tick(); // page cross = +1
        }
    }

    // -------- ALU helpers --------
    fn adc(&mut self, value: u8) {
        let v = value;
        let c = (self.reg_p & P_CARRY) as u16;
        if self.reg_p & P_DECIMAL != 0 {
            let mut lo = (self.reg_a & 0x0f) as i16 + (v & 0x0f) as i16 + c as i16;
            let mut hi = (self.reg_a & 0xf0) as i16 + (v & 0xf0) as i16;
            let bin_result = (self.reg_a as u16 + v as u16 + c) & 0xff;
            self.flag_z = if bin_result == 0 { 0 } else { 1 };
            if lo > 9 {
                hi += 0x10;
                lo += 6;
            }
            self.flag_n = (hi & 0x80) as u8;
            self.set_overflow(
                ((self.reg_a as i16 ^ hi) & 0x80) != 0 && ((self.reg_a ^ v) & 0x80) == 0,
            );
            if hi > 0x90 {
                hi += 0x60;
            }
            self.set_carry((hi & 0xff00u16 as i16) != 0);
            self.reg_a = ((hi & 0xf0) | (lo & 0x0f)) as u8;
            return;
        }
        let result = self.reg_a as u16 + v as u16 + c;
        self.set_carry((result & 0x100) != 0);
        self.set_overflow(
            (self.reg_a & 0x80) == (v & 0x80) && (self.reg_a & 0x80) != ((result as u8) & 0x80),
        );
        self.reg_a = result as u8;
        self.update_nz(self.reg_a);
    }

    fn sbc(&mut self, value: u8) {
        let v = value;
        let c = (self.reg_p & P_CARRY) as i16;
        let bin_result: i16 = self.reg_a as i16 - v as i16 - (1 - c);
        if self.reg_p & P_DECIMAL != 0 {
            let mut lo: i16 = (self.reg_a & 0x0f) as i16 - (v & 0x0f) as i16 - (1 - c);
            let mut hi: i16 = (self.reg_a & 0xf0) as i16 - (v & 0xf0) as i16;
            if lo & 0x10 != 0 {
                lo -= 6;
                hi -= 0x10;
            }
            if hi & 0x100 != 0 {
                hi -= 0x60;
            }
            self.set_carry((bin_result & 0x100) == 0);
            self.set_overflow(
                ((self.reg_a as i16 ^ bin_result) & 0x80) != 0 && ((self.reg_a ^ v) & 0x80) != 0,
            );
            self.reg_a = ((hi & 0xf0) | (lo & 0x0f)) as u8;
            self.update_nz(bin_result as u8);
            return;
        }
        self.set_carry((bin_result & 0x100) == 0);
        self.set_overflow(
            (self.reg_a & 0x80) != (v & 0x80) && (self.reg_a & 0x80) != ((bin_result as u8) & 0x80),
        );
        self.reg_a = bin_result as u8;
        self.update_nz(self.reg_a);
    }

    fn compare(&mut self, reg: u8, value: u8) {
        let result: i16 = reg as i16 - value as i16;
        self.set_carry((result & 0x100) == 0);
        self.update_nz(result as u8);
    }

    fn bit_op(&mut self, value: u8) {
        let v = value;
        self.reg_p &= !P_OVERFLOW;
        self.reg_p |= v & P_OVERFLOW;
        self.flag_n = v & 0x80;
        self.flag_z = if v & self.reg_a == 0 { 0 } else { 1 };
    }

    #[inline]
    fn update_nz(&mut self, v: u8) {
        self.flag_n = v & 0x80;
        self.flag_z = if v == 0 { 0 } else { 1 };
    }
    #[inline]
    fn set_carry(&mut self, b: bool) {
        self.reg_p = (self.reg_p & !P_CARRY) | if b { P_CARRY } else { 0 };
    }
    #[inline]
    fn set_overflow(&mut self, b: bool) {
        self.reg_p = (self.reg_p & !P_OVERFLOW) | if b { P_OVERFLOW } else { 0 };
    }

    // -------- illegal opcodes / JAM --------
    fn execute_illegal_opcode<B: Bus, O: Observer>(
        &mut self,
        bus: &mut B,
        obs: &mut O,
        opcode: u8,
        opcode_pc: u16,
    ) {
        let slot = UNDOC_TABLE[opcode as usize];
        let slot = match slot {
            None => {
                // True KIL/JAM: freeze.
                self.jammed = true;
                self.at_boundary = true;
                self.inst = None;
                self.reg_pc = opcode_pc;
                return;
            }
            Some(s) => s,
        };
        let arg = self.resolve_illegal_arg(bus, obs, slot.mode);
        self.execute_illegal(bus, obs, slot.kind, slot.mode, arg);
        // Burn remaining cycles (cycles - 1) via a no-op micro-op pattern.
        // TS executeIllegalOpcode sets up a burn inst (= remaining cycles). Its
        // executeFinalOp dispatches against `kind` (which is NOT a legal-op name,
        // so the final-op switch is a no-op — the illegal already ran above), and
        // continueInstructionCycle fires onInstructionComplete a SECOND time at
        // burn end with the real operand bytes. We mirror that double-emit so the
        // CpuStep stream matches byte-for-byte.
        let burn = (slot.cycles as usize).saturating_sub(1);
        if burn > 0 {
            self.at_boundary = false;
            let pattern = burn_pattern(slot.cycles as usize);
            let mut inst = self.make_fresh_state(
                MicroEntry { op: slot.kind, mode: slot.mode, cycles: slot.cycles, pattern: "imp" },
                pattern,
                opcode_pc,
                opcode,
            );
            inst.micro_idx = 1;
            self.inst = Some(inst);
        }
    }

    fn resolve_illegal_arg<B: Bus, O: Observer>(
        &mut self,
        bus: &mut B,
        obs: &mut O,
        mode: &str,
    ) -> IllegalArg {
        let mut ea: u16 = 0;
        let mut value: u8 = 0;
        match mode {
            "imp" | "acc" => {}
            "imm" => {
                value = self.load_read(bus, obs, self.reg_pc);
                self.reg_pc = self.reg_pc.wrapping_add(1);
            }
            "zp" => {
                ea = self.load_read(bus, obs, self.reg_pc) as u16;
                self.reg_pc = self.reg_pc.wrapping_add(1);
            }
            "zpx" => {
                ea = (self.load_read(bus, obs, self.reg_pc) as u16 + self.reg_x as u16) & 0xff;
                self.reg_pc = self.reg_pc.wrapping_add(1);
            }
            "zpy" => {
                ea = (self.load_read(bus, obs, self.reg_pc) as u16 + self.reg_y as u16) & 0xff;
                self.reg_pc = self.reg_pc.wrapping_add(1);
            }
            "abs" => {
                let lo = self.load_read(bus, obs, self.reg_pc) as u16;
                self.reg_pc = self.reg_pc.wrapping_add(1);
                let hi = self.load_read(bus, obs, self.reg_pc) as u16;
                self.reg_pc = self.reg_pc.wrapping_add(1);
                ea = lo | (hi << 8);
            }
            "absx" => {
                let lo = self.load_read(bus, obs, self.reg_pc) as u16;
                self.reg_pc = self.reg_pc.wrapping_add(1);
                let hi = self.load_read(bus, obs, self.reg_pc) as u16;
                self.reg_pc = self.reg_pc.wrapping_add(1);
                ea = (lo | (hi << 8)).wrapping_add(self.reg_x as u16);
            }
            "absy" => {
                let lo = self.load_read(bus, obs, self.reg_pc) as u16;
                self.reg_pc = self.reg_pc.wrapping_add(1);
                let hi = self.load_read(bus, obs, self.reg_pc) as u16;
                self.reg_pc = self.reg_pc.wrapping_add(1);
                ea = (lo | (hi << 8)).wrapping_add(self.reg_y as u16);
            }
            "indx" => {
                let zp = (self.load_read(bus, obs, self.reg_pc) as u16 + self.reg_x as u16) & 0xff;
                self.reg_pc = self.reg_pc.wrapping_add(1);
                let lo = self.load_read(bus, obs, zp) as u16;
                let hi = self.load_read(bus, obs, (zp + 1) & 0xff) as u16;
                ea = lo | (hi << 8);
            }
            "indy" => {
                let zp = self.load_read(bus, obs, self.reg_pc) as u16;
                self.reg_pc = self.reg_pc.wrapping_add(1);
                let lo = self.load_read(bus, obs, zp) as u16;
                let hi = self.load_read(bus, obs, (zp + 1) & 0xff) as u16;
                ea = (lo | (hi << 8)).wrapping_add(self.reg_y as u16);
            }
            _ => {}
        }
        IllegalArg { ea, value }
    }

    fn execute_illegal<B: Bus, O: Observer>(
        &mut self,
        bus: &mut B,
        obs: &mut O,
        kind: &str,
        mode: &str,
        arg: IllegalArg,
    ) {
        let v: u8 = if mode == "imm" {
            arg.value
        } else if mode == "imp" || mode == "acc" {
            0
        } else {
            self.load_read(bus, obs, arg.ea)
        };
        match kind {
            "nop" => {}
            "slo" => {
                self.set_carry(v & 0x80 != 0);
                let shifted = v << 1;
                self.store(bus, obs, arg.ea, shifted);
                self.reg_a |= shifted;
                self.update_nz(self.reg_a);
            }
            "rla" => {
                let old_c = self.reg_p & P_CARRY;
                self.set_carry(v & 0x80 != 0);
                let shifted = (v << 1) | if old_c != 0 { 1 } else { 0 };
                self.store(bus, obs, arg.ea, shifted);
                self.reg_a &= shifted;
                self.update_nz(self.reg_a);
            }
            "sre" => {
                self.set_carry(v & 0x01 != 0);
                let shifted = v >> 1;
                self.store(bus, obs, arg.ea, shifted);
                self.reg_a ^= shifted;
                self.update_nz(self.reg_a);
            }
            "rra" => {
                let old_c = self.reg_p & P_CARRY;
                self.set_carry(v & 0x01 != 0);
                let shifted = (v >> 1) | if old_c != 0 { 0x80 } else { 0 };
                self.store(bus, obs, arg.ea, shifted);
                self.adc(shifted);
            }
            "sax" => {
                self.store(bus, obs, arg.ea, self.reg_a & self.reg_x);
            }
            "lax" => {
                self.reg_a = v;
                self.reg_x = v;
                self.update_nz(v);
            }
            "dcp" => {
                let dec = v.wrapping_sub(1);
                self.store(bus, obs, arg.ea, dec);
                let result: i16 = self.reg_a as i16 - dec as i16;
                self.set_carry((result & 0x100) == 0);
                self.update_nz(result as u8);
            }
            "isb" => {
                let inc = v.wrapping_add(1);
                self.store(bus, obs, arg.ea, inc);
                self.sbc(inc);
            }
            "anc" => {
                self.reg_a &= v;
                self.update_nz(self.reg_a);
                self.set_carry(self.reg_a & 0x80 != 0);
            }
            "alr" => {
                self.reg_a &= v;
                self.set_carry(self.reg_a & 0x01 != 0);
                self.reg_a >>= 1;
                self.update_nz(self.reg_a);
            }
            "arr" => {
                let tmp = self.reg_a & v;
                let old_c = self.reg_p & P_CARRY;
                if self.reg_p & P_DECIMAL != 0 {
                    self.reg_a = (tmp >> 1) | if old_c != 0 { 0x80 } else { 0 };
                    self.update_nz(self.reg_a);
                    self.set_overflow(((self.reg_a ^ tmp) & 0x40) != 0);
                    if ((tmp & 0x0f) + (tmp & 0x01)) > 0x05 {
                        self.reg_a = (self.reg_a & 0xf0) | (self.reg_a.wrapping_add(0x06) & 0x0f);
                    }
                    if ((tmp as u16 & 0xf0) + (tmp as u16 & 0x10)) > 0x50 {
                        self.reg_a = self.reg_a.wrapping_add(0x60);
                        self.set_carry(true);
                    } else {
                        self.set_carry(false);
                    }
                } else {
                    self.reg_a = (tmp >> 1) | if old_c != 0 { 0x80 } else { 0 };
                    self.update_nz(self.reg_a);
                    self.set_carry(self.reg_a & 0x40 != 0);
                    self.set_overflow(((self.reg_a & 0x40) ^ ((self.reg_a & 0x20) << 1)) != 0);
                }
            }
            "xaa" => {
                self.reg_a = (self.reg_a | 0xee) & self.reg_x & v;
                self.update_nz(self.reg_a);
            }
            "axs" => {
                let result: i16 = (self.reg_a & self.reg_x) as i16 - v as i16;
                self.set_carry((result & 0x100) == 0);
                self.reg_x = result as u8;
                self.update_nz(self.reg_x);
            }
            "sbc_imm" => self.sbc(v),
            "shy" => {
                let val = self.reg_y & (((arg.ea >> 8) as u8).wrapping_add(1));
                self.store(bus, obs, arg.ea, val);
            }
            "shx" => {
                let val = self.reg_x & (((arg.ea >> 8) as u8).wrapping_add(1));
                self.store(bus, obs, arg.ea, val);
            }
            "ahx" => {
                let val = self.reg_a & self.reg_x & (((arg.ea >> 8) as u8).wrapping_add(1));
                self.store(bus, obs, arg.ea, val);
            }
            "tas" => {
                self.reg_sp = self.reg_a & self.reg_x;
                let val = self.reg_sp & (((arg.ea >> 8) as u8).wrapping_add(1));
                self.store(bus, obs, arg.ea, val);
            }
            "las" => {
                let r = v & self.reg_sp;
                self.reg_a = r;
                self.reg_x = r;
                self.reg_sp = r;
                self.update_nz(r);
            }
            _ => {}
        }
    }
}

struct IllegalArg {
    ea: u16,
    value: u8,
}

/// Operand byte count per addressing mode (= TS `operandByteCount`).
fn operand_byte_count(mode: &str) -> usize {
    match mode {
        "abs" | "absx" | "absy" | "ind" => 2,
        "imp" | "impl" | "implied" | "acc" | "accumulator" | "" => 0,
        _ => 1,
    }
}

/// Burn pattern: fetch_opcode + (cycles-1) internals (= TS `makeBurnPattern`).
fn burn_pattern(cycles: usize) -> &'static [&'static str] {
    const PATS: [&[&str]; 9] = [
        &["fetch_opcode"],
        &["fetch_opcode"],
        &["fetch_opcode", "internal"],
        &["fetch_opcode", "internal", "internal"],
        &["fetch_opcode", "internal", "internal", "internal"],
        &["fetch_opcode", "internal", "internal", "internal", "internal"],
        &["fetch_opcode", "internal", "internal", "internal", "internal", "internal"],
        &["fetch_opcode", "internal", "internal", "internal", "internal", "internal", "internal"],
        &["fetch_opcode", "internal", "internal", "internal", "internal", "internal", "internal", "internal"],
    ];
    PATS[cycles.min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NullSink;

    struct Ram {
        mem: Box<[u8; 0x10000]>,
    }
    impl Ram {
        fn new() -> Self {
            Self { mem: Box::new([0u8; 0x10000]) }
        }
    }
    impl Bus for Ram {
        fn read(&mut self, addr: u16) -> u8 {
            self.mem[addr as usize]
        }
        fn write(&mut self, addr: u16, value: u8) {
            self.mem[addr as usize] = value;
        }
    }

    /// Load a program at `org`, set PC, run exactly one instruction. Returns clk.
    fn run_one(cpu: &mut Cpu6510, ram: &mut Ram) {
        let mut obs = NullSink;
        loop {
            cpu.execute_cycle(ram, &mut obs);
            if cpu.is_at_boundary() {
                break;
            }
        }
    }

    fn setup(prog: &[u8], org: u16) -> (Cpu6510, Ram) {
        let mut cpu = Cpu6510::new();
        let mut ram = Ram::new();
        for (i, b) in prog.iter().enumerate() {
            ram.mem[org as usize + i] = *b;
        }
        cpu.reg_pc = org;
        (cpu, ram)
    }

    #[test]
    fn lda_imm_sets_a_and_z_n_and_2_cycles() {
        let (mut c, mut r) = setup(&[0xa9, 0x00], 0x1000); // LDA #$00
        run_one(&mut c, &mut r);
        assert_eq!(c.reg_a, 0x00);
        assert_eq!(c.flag_z, 0, "Z set");
        assert_eq!(c.clk, 2);
        assert_eq!(c.reg_pc, 0x1002);

        let (mut c, mut r) = setup(&[0xa9, 0x80], 0x1000); // LDA #$80
        run_one(&mut c, &mut r);
        assert_eq!(c.reg_a, 0x80);
        assert_eq!(c.flag_n, 0x80, "N set");
        assert_eq!(c.clk, 2);
    }

    #[test]
    fn lda_abs_4_cycles() {
        let (mut c, mut r) = setup(&[0xad, 0x34, 0x12], 0x1000); // LDA $1234
        r.mem[0x1234] = 0x42;
        run_one(&mut c, &mut r);
        assert_eq!(c.reg_a, 0x42);
        assert_eq!(c.clk, 4);
    }

    #[test]
    fn lda_absx_page_cross_adds_cycle() {
        // LDA $12F0,X with X=$20 -> $1310 crosses page -> 5 cycles
        let (mut c, mut r) = setup(&[0xa2, 0x20, 0xbd, 0xf0, 0x12], 0x1000);
        r.mem[0x1310] = 0x55;
        run_one(&mut c, &mut r); // LDX #$20 (2 cyc)
        assert_eq!(c.clk, 2);
        run_one(&mut c, &mut r); // LDA $12F0,X
        assert_eq!(c.reg_a, 0x55);
        assert_eq!(c.clk, 2 + 5);
    }

    #[test]
    fn lda_absx_no_page_cross_4_cycles() {
        let (mut c, mut r) = setup(&[0xa2, 0x04, 0xbd, 0x00, 0x12], 0x1000);
        r.mem[0x1204] = 0x66;
        run_one(&mut c, &mut r); // LDX #$04
        run_one(&mut c, &mut r); // LDA $1200,X
        assert_eq!(c.reg_a, 0x66);
        assert_eq!(c.clk, 2 + 4);
    }

    #[test]
    fn sta_zp_writes_3_cycles() {
        let (mut c, mut r) = setup(&[0xa9, 0xab, 0x85, 0x10], 0x1000); // LDA #$AB; STA $10
        run_one(&mut c, &mut r);
        run_one(&mut c, &mut r);
        assert_eq!(r.mem[0x10], 0xab);
        assert_eq!(c.clk, 2 + 3);
    }

    #[test]
    fn adc_with_carry() {
        // CLC; LDA #$10; ADC #$05 -> $15, no carry
        let (mut c, mut r) = setup(&[0x18, 0xa9, 0x10, 0x69, 0x05], 0x1000);
        run_one(&mut c, &mut r); // CLC
        run_one(&mut c, &mut r); // LDA
        run_one(&mut c, &mut r); // ADC
        assert_eq!(c.reg_a, 0x15);
        assert_eq!(c.reg_p & P_CARRY, 0);
    }

    #[test]
    fn adc_overflow_and_carry() {
        // CLC; LDA #$50; ADC #$50 -> $A0, V set, N set, C clear
        let (mut c, mut r) = setup(&[0x18, 0xa9, 0x50, 0x69, 0x50], 0x1000);
        run_one(&mut c, &mut r);
        run_one(&mut c, &mut r);
        run_one(&mut c, &mut r);
        assert_eq!(c.reg_a, 0xa0);
        assert_eq!(c.reg_p & P_OVERFLOW, P_OVERFLOW, "V set");
        assert_eq!(c.flag_n, 0x80, "N set");
        assert_eq!(c.reg_p & P_CARRY, 0, "C clear");
    }

    #[test]
    fn sbc_basic() {
        // SEC; LDA #$50; SBC #$30 -> $20, C set
        let (mut c, mut r) = setup(&[0x38, 0xa9, 0x50, 0xe9, 0x30], 0x1000);
        run_one(&mut c, &mut r);
        run_one(&mut c, &mut r);
        run_one(&mut c, &mut r);
        assert_eq!(c.reg_a, 0x20);
        assert_eq!(c.reg_p & P_CARRY, P_CARRY);
    }

    #[test]
    fn branch_taken_adds_cycle_no_pagecross() {
        // LDA #$00 (Z set); BEQ +2 -> taken, 3 cycles
        let (mut c, mut r) = setup(&[0xa9, 0x00, 0xf0, 0x02], 0x1000);
        run_one(&mut c, &mut r); // LDA
        let before = c.clk;
        run_one(&mut c, &mut r); // BEQ taken
        assert_eq!(c.clk - before, 3, "branch taken no pagecross = 3");
        assert_eq!(c.reg_pc, 0x1004 + 2);
    }

    #[test]
    fn branch_not_taken_2_cycles() {
        // LDA #$01 (Z clear); BEQ +2 -> not taken, 2 cycles
        let (mut c, mut r) = setup(&[0xa9, 0x01, 0xf0, 0x02], 0x1000);
        run_one(&mut c, &mut r);
        let before = c.clk;
        run_one(&mut c, &mut r);
        assert_eq!(c.clk - before, 2);
        assert_eq!(c.reg_pc, 0x1004);
    }

    #[test]
    fn jsr_rts_roundtrip() {
        // JSR $2000 ; (at $2000) RTS
        let (mut c, mut r) = setup(&[0x20, 0x00, 0x20], 0x1000);
        r.mem[0x2000] = 0x60; // RTS
        run_one(&mut c, &mut r); // JSR (6 cyc)
        assert_eq!(c.reg_pc, 0x2000);
        assert_eq!(c.clk, 6);
        assert_eq!(c.reg_sp, 0xfd);
        run_one(&mut c, &mut r); // RTS (6 cyc)
        assert_eq!(c.reg_pc, 0x1003);
        assert_eq!(c.clk, 12);
        assert_eq!(c.reg_sp, 0xff);
    }

    #[test]
    fn php_plp_stack_roundtrip() {
        // SEC; PHP; CLC; PLP -> C restored set
        let (mut c, mut r) = setup(&[0x38, 0x08, 0x18, 0x28], 0x1000);
        run_one(&mut c, &mut r); // SEC
        run_one(&mut c, &mut r); // PHP (3 cyc)
        run_one(&mut c, &mut r); // CLC
        assert_eq!(c.reg_p & P_CARRY, 0);
        run_one(&mut c, &mut r); // PLP (4 cyc)
        assert_eq!(c.reg_p & P_CARRY, P_CARRY, "C restored");
    }

    #[test]
    fn inc_zp_rmw_5_cycles() {
        let (mut c, mut r) = setup(&[0xe6, 0x10], 0x1000); // INC $10
        r.mem[0x10] = 0x0f;
        run_one(&mut c, &mut r);
        assert_eq!(r.mem[0x10], 0x10);
        assert_eq!(c.clk, 5);
    }

    #[test]
    fn asl_acc_2_cycles() {
        // LDA #$40; ASL A -> $80, N set, C clear
        let (mut c, mut r) = setup(&[0xa9, 0x40, 0x0a], 0x1000);
        run_one(&mut c, &mut r);
        run_one(&mut c, &mut r);
        assert_eq!(c.reg_a, 0x80);
        assert_eq!(c.flag_n, 0x80);
        assert_eq!(c.reg_p & P_CARRY, 0);
    }

    #[test]
    fn transfers() {
        // LDX #$7F; TXA -> A=$7F
        let (mut c, mut r) = setup(&[0xa2, 0x7f, 0x8a], 0x1000);
        run_one(&mut c, &mut r);
        run_one(&mut c, &mut r);
        assert_eq!(c.reg_a, 0x7f);
    }

    #[test]
    fn lax_illegal_loads_a_and_x() {
        // LAX $10 (A7) -> A=X=mem
        let (mut c, mut r) = setup(&[0xa7, 0x10], 0x1000);
        r.mem[0x10] = 0x99;
        run_one(&mut c, &mut r);
        assert_eq!(c.reg_a, 0x99);
        assert_eq!(c.reg_x, 0x99);
        assert_eq!(c.clk, 3);
    }

    #[test]
    fn nop_illegal_imm_2_cycles() {
        let (mut c, mut r) = setup(&[0x80, 0x12], 0x1000); // NOP #$12 (illegal)
        run_one(&mut c, &mut r);
        assert_eq!(c.clk, 2);
        assert_eq!(c.reg_pc, 0x1002);
    }

    #[test]
    fn indy_load() {
        // LDY #$00; LDA ($10),Y with ptr $10/$11 -> $2000, Y=0
        let (mut c, mut r) = setup(&[0xa0, 0x00, 0xb1, 0x10], 0x1000);
        r.mem[0x10] = 0x00;
        r.mem[0x11] = 0x20;
        r.mem[0x2000] = 0x77;
        run_one(&mut c, &mut r); // LDY
        run_one(&mut c, &mut r); // LDA (zp),Y  5 cyc no cross
        assert_eq!(c.reg_a, 0x77);
        assert_eq!(c.clk, 2 + 5);
    }
}
