//! Opcode tables — direct port of cpu/microcode-table.ts + cpu/undoc-table.ts.
//!
//! The microcode engine drives addressing modes via per-pattern micro-op lists;
//! the CPU core (cpu.rs) interprets those op strings exactly as the TS
//! `executeMicroOp` switch does. Tables are `const` and deterministic.

/// One legal-opcode microcode entry (= TS `MicrocodeEntry`).
#[derive(Clone, Copy, Debug)]
pub struct MicroEntry {
    pub op: &'static str,
    pub mode: &'static str,
    pub cycles: u8,
    pub pattern: &'static str,
}

const fn e(op: &'static str, mode: &'static str, cycles: u8, pattern: &'static str) -> Option<MicroEntry> {
    Some(MicroEntry { op, mode, cycles, pattern })
}

/// 256-entry legal opcode table. `None` = illegal (handled via UNDOC_TABLE / JAM).
/// Mirrors TS `MICROCODE_TABLE` index-for-index.
pub static MICROCODE_TABLE: [Option<MicroEntry>; 256] = build_microcode();

const fn build_microcode() -> [Option<MicroEntry>; 256] {
    let mut t: [Option<MicroEntry>; 256] = [None; 256];
    t[0x00] = e("brk", "imp", 7, "brk");
    t[0x01] = e("ora", "indx", 6, "indx_read");
    t[0x05] = e("ora", "zp", 3, "zp_read");
    t[0x06] = e("asl", "zp", 5, "zp_rmw");
    t[0x08] = e("php", "imp", 3, "push");
    t[0x09] = e("ora", "imm", 2, "imm");
    t[0x0a] = e("asl", "acc", 2, "acc");
    t[0x0d] = e("ora", "abs", 4, "abs_read");
    t[0x0e] = e("asl", "abs", 6, "abs_rmw");
    t[0x10] = e("bpl", "rel", 2, "rel");
    t[0x11] = e("ora", "indy", 5, "indy_read");
    t[0x15] = e("ora", "zpx", 4, "zpx_read");
    t[0x16] = e("asl", "zpx", 6, "zpx_rmw");
    t[0x18] = e("clc", "imp", 2, "imp");
    t[0x19] = e("ora", "absy", 4, "absy_read");
    t[0x1d] = e("ora", "absx", 4, "absx_read");
    t[0x1e] = e("asl", "absx", 7, "absx_rmw");
    t[0x20] = e("jsr", "abs", 6, "jsr");
    t[0x21] = e("and", "indx", 6, "indx_read");
    t[0x24] = e("bit", "zp", 3, "zp_read");
    t[0x25] = e("and", "zp", 3, "zp_read");
    t[0x26] = e("rol", "zp", 5, "zp_rmw");
    t[0x28] = e("plp", "imp", 4, "pop");
    t[0x29] = e("and", "imm", 2, "imm");
    t[0x2a] = e("rol", "acc", 2, "acc");
    t[0x2c] = e("bit", "abs", 4, "abs_read");
    t[0x2d] = e("and", "abs", 4, "abs_read");
    t[0x2e] = e("rol", "abs", 6, "abs_rmw");
    t[0x30] = e("bmi", "rel", 2, "rel");
    t[0x31] = e("and", "indy", 5, "indy_read");
    t[0x35] = e("and", "zpx", 4, "zpx_read");
    t[0x36] = e("rol", "zpx", 6, "zpx_rmw");
    t[0x38] = e("sec", "imp", 2, "imp");
    t[0x39] = e("and", "absy", 4, "absy_read");
    t[0x3d] = e("and", "absx", 4, "absx_read");
    t[0x3e] = e("rol", "absx", 7, "absx_rmw");
    t[0x40] = e("rti", "imp", 6, "rti");
    t[0x41] = e("eor", "indx", 6, "indx_read");
    t[0x45] = e("eor", "zp", 4, "zp_read");
    t[0x46] = e("lsr", "zp", 5, "zp_rmw");
    t[0x48] = e("pha", "imp", 3, "push");
    t[0x49] = e("eor", "imm", 2, "imm");
    t[0x4a] = e("lsr", "acc", 2, "acc");
    t[0x4c] = e("jmp", "abs", 3, "jmp_abs");
    t[0x4d] = e("eor", "abs", 4, "abs_read");
    t[0x4e] = e("lsr", "abs", 6, "abs_rmw");
    t[0x50] = e("bvc", "rel", 2, "rel");
    t[0x51] = e("eor", "indy", 5, "indy_read");
    t[0x55] = e("eor", "zpx", 3, "zpx_read");
    t[0x56] = e("lsr", "zpx", 6, "zpx_rmw");
    t[0x58] = e("cli", "imp", 2, "imp");
    t[0x59] = e("eor", "absy", 4, "absy_read");
    t[0x5d] = e("eor", "absx", 4, "absx_read");
    t[0x5e] = e("lsr", "absx", 7, "absx_rmw");
    t[0x60] = e("rts", "imp", 6, "rts");
    t[0x61] = e("adc", "indx", 6, "indx_read");
    t[0x65] = e("adc", "zp", 3, "zp_read");
    t[0x66] = e("ror", "zp", 5, "zp_rmw");
    t[0x68] = e("pla", "imp", 4, "pop");
    t[0x69] = e("adc", "imm", 2, "imm");
    t[0x6a] = e("ror", "acc", 2, "acc");
    t[0x6c] = e("jmp", "ind", 5, "ind_jmp");
    t[0x6d] = e("adc", "abs", 4, "abs_read");
    t[0x6e] = e("ror", "abs", 6, "abs_rmw");
    t[0x70] = e("bvs", "rel", 2, "rel");
    t[0x71] = e("adc", "indy", 5, "indy_read");
    t[0x75] = e("adc", "zpx", 4, "zpx_read");
    t[0x76] = e("ror", "zpx", 6, "zpx_rmw");
    t[0x78] = e("sei", "imp", 2, "imp");
    t[0x79] = e("adc", "absy", 4, "absy_read");
    t[0x7d] = e("adc", "absx", 4, "absx_read");
    t[0x7e] = e("ror", "absx", 7, "absx_rmw");
    t[0x81] = e("sta", "indx", 6, "indx_write");
    t[0x84] = e("sty", "zp", 3, "zp_write");
    t[0x85] = e("sta", "zp", 3, "zp_write");
    t[0x86] = e("stx", "zp", 3, "zp_write");
    t[0x88] = e("dey", "imp", 2, "imp");
    t[0x8a] = e("txa", "imp", 2, "imp");
    t[0x8c] = e("sty", "abs", 4, "abs_write");
    t[0x8d] = e("sta", "abs", 4, "abs_write");
    t[0x8e] = e("stx", "abs", 4, "abs_write");
    t[0x90] = e("bcc", "rel", 2, "rel");
    t[0x91] = e("sta", "indy", 6, "indy_write");
    t[0x94] = e("sty", "zpx", 4, "zpx_write");
    t[0x95] = e("sta", "zpx", 4, "zpx_write");
    t[0x96] = e("stx", "zpy", 4, "zpy_write");
    t[0x98] = e("tya", "imp", 2, "imp");
    t[0x99] = e("sta", "absy", 5, "absy_write");
    t[0x9a] = e("txs", "imp", 2, "imp");
    t[0x9d] = e("sta", "absx", 5, "absx_write");
    t[0xa0] = e("ldy", "imm", 2, "imm");
    t[0xa1] = e("lda", "indx", 6, "indx_read");
    t[0xa2] = e("ldx", "imm", 2, "imm");
    t[0xa4] = e("ldy", "zp", 3, "zp_read");
    t[0xa5] = e("lda", "zp", 3, "zp_read");
    t[0xa6] = e("ldx", "zp", 3, "zp_read");
    t[0xa8] = e("tay", "imp", 2, "imp");
    t[0xa9] = e("lda", "imm", 2, "imm");
    t[0xaa] = e("tax", "imp", 2, "imp");
    t[0xac] = e("ldy", "abs", 4, "abs_read");
    t[0xad] = e("lda", "abs", 4, "abs_read");
    t[0xae] = e("ldx", "abs", 4, "abs_read");
    t[0xb0] = e("bcs", "rel", 2, "rel");
    t[0xb1] = e("lda", "indy", 5, "indy_read");
    t[0xb4] = e("ldy", "zpx", 4, "zpx_read");
    t[0xb5] = e("lda", "zpx", 4, "zpx_read");
    t[0xb6] = e("ldx", "zpy", 4, "zpy_read");
    t[0xb8] = e("clv", "imp", 2, "imp");
    t[0xb9] = e("lda", "absy", 4, "absy_read");
    t[0xba] = e("tsx", "imp", 2, "imp");
    t[0xbc] = e("ldy", "absx", 4, "absx_read");
    t[0xbd] = e("lda", "absx", 4, "absx_read");
    t[0xbe] = e("ldx", "absy", 4, "absy_read");
    t[0xc0] = e("cpy", "imm", 2, "imm");
    t[0xc1] = e("cmp", "indx", 6, "indx_read");
    t[0xc4] = e("cpy", "zp", 3, "zp_read");
    t[0xc5] = e("cmp", "zp", 3, "zp_read");
    t[0xc6] = e("dec", "zp", 5, "zp_rmw");
    t[0xc8] = e("iny", "imp", 2, "imp");
    t[0xc9] = e("cmp", "imm", 2, "imm");
    t[0xca] = e("dex", "imp", 2, "imp");
    t[0xcc] = e("cpy", "abs", 4, "abs_read");
    t[0xcd] = e("cmp", "abs", 4, "abs_read");
    t[0xce] = e("dec", "abs", 6, "abs_rmw");
    t[0xd0] = e("bne", "rel", 2, "rel");
    t[0xd1] = e("cmp", "indy", 5, "indy_read");
    t[0xd5] = e("cmp", "zpx", 4, "zpx_read");
    t[0xd6] = e("dec", "zpx", 6, "zpx_rmw");
    t[0xd8] = e("cld", "imp", 2, "imp");
    t[0xd9] = e("cmp", "absy", 4, "absy_read");
    t[0xdd] = e("cmp", "absx", 4, "absx_read");
    t[0xde] = e("dec", "absx", 7, "absx_rmw");
    t[0xe0] = e("cpx", "imm", 2, "imm");
    t[0xe1] = e("sbc", "indx", 6, "indx_read");
    t[0xe4] = e("cpx", "zp", 3, "zp_read");
    t[0xe5] = e("sbc", "zp", 3, "zp_read");
    t[0xe6] = e("inc", "zp", 5, "zp_rmw");
    t[0xe8] = e("inx", "imp", 2, "imp");
    t[0xe9] = e("sbc", "imm", 2, "imm");
    t[0xea] = e("nop", "imp", 2, "imp");
    t[0xec] = e("cpx", "abs", 4, "abs_read");
    t[0xed] = e("sbc", "abs", 4, "abs_read");
    t[0xee] = e("inc", "abs", 6, "abs_rmw");
    t[0xf0] = e("beq", "rel", 2, "rel");
    t[0xf1] = e("sbc", "indy", 5, "indy_read");
    t[0xf5] = e("sbc", "zpx", 4, "zpx_read");
    t[0xf6] = e("inc", "zpx", 6, "zpx_rmw");
    t[0xf8] = e("sed", "imp", 2, "imp");
    t[0xf9] = e("sbc", "absy", 4, "absy_read");
    t[0xfd] = e("sbc", "absx", 4, "absx_read");
    t[0xfe] = e("inc", "absx", 7, "absx_rmw");
    t
}

/// Resolve a micro-op pattern name to its op-string sequence (= TS
/// `ADDR_MODE_PATTERNS`).
pub fn addr_mode_pattern(pattern: &str) -> &'static [&'static str] {
    match pattern {
        "imp" => &["fetch_opcode", "internal"],
        "acc" => &["fetch_opcode", "internal"],
        "imm" => &["fetch_opcode", "fetch_imm"],
        "zp_read" => &["fetch_opcode", "fetch_zp_lo", "read_ea"],
        "zp_write" => &["fetch_opcode", "fetch_zp_lo", "write_ea"],
        "zp_rmw" => &["fetch_opcode", "fetch_zp_lo", "read_ea", "dummy_write_ea_old", "write_ea_new"],
        "zpx_read" => &["fetch_opcode", "fetch_zp_lo", "dummy_zp", "read_ea"],
        "zpx_write" => &["fetch_opcode", "fetch_zp_lo", "dummy_zp", "write_ea"],
        "zpx_rmw" => &["fetch_opcode", "fetch_zp_lo", "dummy_zp", "read_ea", "dummy_write_ea_old", "write_ea_new"],
        "zpy_read" => &["fetch_opcode", "fetch_zp_lo", "dummy_zp", "read_ea"],
        "zpy_write" => &["fetch_opcode", "fetch_zp_lo", "dummy_zp", "write_ea"],
        "abs_read" => &["fetch_opcode", "fetch_lo", "fetch_hi", "read_ea"],
        "abs_write" => &["fetch_opcode", "fetch_lo", "fetch_hi", "write_ea"],
        "abs_rmw" => &["fetch_opcode", "fetch_lo", "fetch_hi", "read_ea", "dummy_write_ea_old", "write_ea_new"],
        "absx_read" => &["fetch_opcode", "fetch_lo", "fetch_hi", "read_ea_pgx"],
        "absx_write" => &["fetch_opcode", "fetch_lo", "fetch_hi", "dummy_addr", "write_ea"],
        "absx_rmw" => &["fetch_opcode", "fetch_lo", "fetch_hi", "dummy_addr", "read_ea", "dummy_write_ea_old", "write_ea_new"],
        "absy_read" => &["fetch_opcode", "fetch_lo", "fetch_hi", "read_ea_pgy"],
        "absy_write" => &["fetch_opcode", "fetch_lo", "fetch_hi", "dummy_addr", "write_ea"],
        "ind_jmp" => &["fetch_opcode", "fetch_lo", "fetch_hi", "read_ea_lo", "read_ea_hi"],
        "indx_read" => &["fetch_opcode", "fetch_zp_lo", "dummy_zp", "fetch_ind_lo", "fetch_ind_hi", "read_ea"],
        "indx_write" => &["fetch_opcode", "fetch_zp_lo", "dummy_zp", "fetch_ind_lo", "fetch_ind_hi", "write_ea"],
        "indx_rmw" => &["fetch_opcode", "fetch_zp_lo", "dummy_zp", "fetch_ind_lo", "fetch_ind_hi", "read_ea", "dummy_write_ea_old", "write_ea_new"],
        "indy_read" => &["fetch_opcode", "fetch_zp_lo", "fetch_ind_lo", "fetch_ind_hi", "read_ea_pgy"],
        "indy_write" => &["fetch_opcode", "fetch_zp_lo", "fetch_ind_lo", "fetch_ind_hi", "dummy_addr", "write_ea"],
        "indy_rmw" => &["fetch_opcode", "fetch_zp_lo", "fetch_ind_lo", "fetch_ind_hi", "dummy_addr", "read_ea", "dummy_write_ea_old", "write_ea_new"],
        "rel" => &["fetch_opcode", "fetch_imm"],
        "push" => &["fetch_opcode", "internal", "push"],
        "pop" => &["fetch_opcode", "internal", "dummy_sp", "pop"],
        "brk" => &["fetch_opcode", "fetch_dummy_pc", "push_pch", "push_pcl", "push_p_brk", "read_brk_vec_lo", "read_brk_vec_hi"],
        "rti" => &["fetch_opcode", "internal", "dummy_sp", "pop_p", "pop_pcl", "pop_pch"],
        "rts" => &["fetch_opcode", "internal", "dummy_sp", "pop_pcl", "pop_pch", "fetch_pc_dummy"],
        "jsr" => &["fetch_opcode", "fetch_lo", "dummy_sp", "push_pch", "push_pcl", "fetch_hi"],
        "jmp_abs" => &["fetch_opcode", "fetch_lo", "fetch_hi"],
        _ => &["fetch_opcode", "internal"],
    }
}

/// Illegal-opcode slot (= TS `UndocSlot`).
#[derive(Clone, Copy, Debug)]
pub struct UndocSlot {
    pub kind: &'static str,
    pub mode: &'static str,
    pub cycles: u8,
}

const fn u(kind: &'static str, mode: &'static str, cycles: u8) -> Option<UndocSlot> {
    Some(UndocSlot { kind, mode, cycles })
}

/// 256-entry illegal-opcode table (= TS `UNDOC_TABLE`). `None` here AND `None`
/// in MICROCODE_TABLE = a true JAM/KIL opcode.
pub static UNDOC_TABLE: [Option<UndocSlot>; 256] = build_undoc();

const fn build_undoc() -> [Option<UndocSlot>; 256] {
    let mut t: [Option<UndocSlot>; 256] = [None; 256];
    // nop imp
    t[0x1a] = u("nop", "imp", 2); t[0x3a] = u("nop", "imp", 2); t[0x5a] = u("nop", "imp", 2);
    t[0x7a] = u("nop", "imp", 2); t[0xda] = u("nop", "imp", 2); t[0xfa] = u("nop", "imp", 2);
    // nop imm
    t[0x80] = u("nop", "imm", 2); t[0x82] = u("nop", "imm", 2); t[0x89] = u("nop", "imm", 2);
    t[0xc2] = u("nop", "imm", 2); t[0xe2] = u("nop", "imm", 2);
    // nop zp
    t[0x04] = u("nop", "zp", 3); t[0x44] = u("nop", "zp", 3); t[0x64] = u("nop", "zp", 3);
    // nop zpx
    t[0x14] = u("nop", "zpx", 4); t[0x34] = u("nop", "zpx", 4); t[0x54] = u("nop", "zpx", 4);
    t[0x74] = u("nop", "zpx", 4); t[0xd4] = u("nop", "zpx", 4); t[0xf4] = u("nop", "zpx", 4);
    // nop abs
    t[0x0c] = u("nop", "abs", 4);
    // nop absx
    t[0x1c] = u("nop", "absx", 4); t[0x3c] = u("nop", "absx", 4); t[0x5c] = u("nop", "absx", 4);
    t[0x7c] = u("nop", "absx", 4); t[0xdc] = u("nop", "absx", 4); t[0xfc] = u("nop", "absx", 4);
    // slo
    t[0x07] = u("slo", "zp", 5); t[0x17] = u("slo", "zpx", 6);
    t[0x0f] = u("slo", "abs", 6); t[0x1f] = u("slo", "absx", 7);
    t[0x1b] = u("slo", "absy", 7); t[0x03] = u("slo", "indx", 8); t[0x13] = u("slo", "indy", 8);
    // rla
    t[0x27] = u("rla", "zp", 5); t[0x37] = u("rla", "zpx", 6);
    t[0x2f] = u("rla", "abs", 6); t[0x3f] = u("rla", "absx", 7);
    t[0x3b] = u("rla", "absy", 7); t[0x23] = u("rla", "indx", 8); t[0x33] = u("rla", "indy", 8);
    // sre
    t[0x47] = u("sre", "zp", 5); t[0x57] = u("sre", "zpx", 6);
    t[0x4f] = u("sre", "abs", 6); t[0x5f] = u("sre", "absx", 7);
    t[0x5b] = u("sre", "absy", 7); t[0x43] = u("sre", "indx", 8); t[0x53] = u("sre", "indy", 8);
    // rra
    t[0x67] = u("rra", "zp", 5); t[0x77] = u("rra", "zpx", 6);
    t[0x6f] = u("rra", "abs", 6); t[0x7f] = u("rra", "absx", 7);
    t[0x7b] = u("rra", "absy", 7); t[0x63] = u("rra", "indx", 8); t[0x73] = u("rra", "indy", 8);
    // sax
    t[0x87] = u("sax", "zp", 3); t[0x97] = u("sax", "zpy", 4);
    t[0x8f] = u("sax", "abs", 4); t[0x83] = u("sax", "indx", 6);
    // lax
    t[0xa7] = u("lax", "zp", 3); t[0xb7] = u("lax", "zpy", 4);
    t[0xaf] = u("lax", "abs", 4); t[0xbf] = u("lax", "absy", 4);
    t[0xa3] = u("lax", "indx", 6); t[0xb3] = u("lax", "indy", 5);
    t[0xab] = u("lax", "imm", 2);
    // dcp
    t[0xc7] = u("dcp", "zp", 5); t[0xd7] = u("dcp", "zpx", 6);
    t[0xcf] = u("dcp", "abs", 6); t[0xdf] = u("dcp", "absx", 7);
    t[0xdb] = u("dcp", "absy", 7); t[0xc3] = u("dcp", "indx", 8); t[0xd3] = u("dcp", "indy", 8);
    // isb
    t[0xe7] = u("isb", "zp", 5); t[0xf7] = u("isb", "zpx", 6);
    t[0xef] = u("isb", "abs", 6); t[0xff] = u("isb", "absx", 7);
    t[0xfb] = u("isb", "absy", 7); t[0xe3] = u("isb", "indx", 8); t[0xf3] = u("isb", "indy", 8);
    // immediate ALU illegals
    t[0x0b] = u("anc", "imm", 2); t[0x2b] = u("anc", "imm", 2);
    t[0x4b] = u("alr", "imm", 2);
    t[0x6b] = u("arr", "imm", 2);
    t[0x8b] = u("xaa", "imm", 2);
    t[0xcb] = u("axs", "imm", 2);
    t[0xeb] = u("sbc_imm", "imm", 2);
    // store-high illegals
    t[0x9c] = u("shy", "absx", 5);
    t[0x9e] = u("shx", "absy", 5);
    t[0x93] = u("ahx", "indy", 6);
    t[0x9f] = u("ahx", "absy", 5);
    t[0x9b] = u("tas", "absy", 5);
    t[0xbb] = u("las", "absy", 4);
    t
}
