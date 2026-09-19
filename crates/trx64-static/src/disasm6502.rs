//! 6502/6510 static disassembler — extracted from trx64-daemon `main.rs`
//! (capability-cut migration step 1) so every consumer shares one decoder.
//!
//! Two formatters, two contracts — do NOT "unify" them:
//!
//!  - [`disasm_line_ts`] — 1:1 port of the C64RE TS oracle `disasm6502.ts`
//!    `disasmLine`: `$addr  bb bb bb  MNEMONIC ops`, bytes padded to a fixed
//!    8-char column, mnemonic upper-cased, operand hex LOWER-case (VICE-ish).
//!    Golden-tested byte-identical vs the TS oracle (`tests/goldens/`).
//!    [`disasm_line_ts_spans`] is the same line plus WHERE it printed each address
//!    (Spec 804): the runtime says where its addresses are, the client names them.
//!    There is no labelled variant any more — names are joined in C64RE.
//!  - [`disasm_one`] — the `monitorDisasm` api/call shape (UPPERCASE hex,
//!    `.byte $XX` fallback for true JAM holes). This is the daemon's MCP wire
//!    contract, moved verbatim; its text convention intentionally differs.

use trx64_core::tables::{MICROCODE_TABLE, UNDOC_TABLE};

/// Mnemonic + addressing mode in the TS-oracle (`disasm6502.ts` OPCODES) naming.
///
/// The trx64-core tables carry execution-oriented names for two undocumented
/// ops where the disasm oracle prints the conventional mnemonic: `isb` → `isc`,
/// `sbc_imm` → `sbc`. A hole in BOTH tables is a true JAM/KIL byte — the TS
/// oracle defines all 12 JAM opcodes as `jam` (size 1), so that's what we
/// print. (The old daemon-local copy rendered `???` / `ISB` / `SBC_IMM` here —
/// a latent parity gap vs its declared oracle, closed by the golden tests.)
fn mnemonic_mode_ts(opcode: u8) -> (&'static str, &'static str) {
    if let Some(e) = MICROCODE_TABLE[opcode as usize] {
        return (e.op, e.mode);
    }
    if let Some(u) = UNDOC_TABLE[opcode as usize] {
        let mne = match u.kind {
            "isb" => "isc",
            "sbc_imm" => "sbc",
            k => k,
        };
        return (mne, u.mode);
    }
    ("jam", "imp")
}

/// Instruction length in bytes (1–3) from the addressing mode; JAM holes are 1.
pub fn instr_len(opcode: u8) -> usize {
    let mode = MICROCODE_TABLE[opcode as usize]
        .map(|e| e.mode)
        .or_else(|| UNDOC_TABLE[opcode as usize].map(|e| e.mode));
    match mode {
        Some("imp") | Some("acc") => 1,
        Some("imm") | Some("zp") | Some("zpx") | Some("zpy")
        | Some("indx") | Some("indy") | Some("rel") => 2,
        Some("abs") | Some("absx") | Some("absy") | Some("ind") => 3,
        _ => 1, // JAM hole: 1 byte
    }
}

/// Spec 804 — what an address printed in a disassembly line IS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanRole {
    /// The instruction's own address.
    Pc,
    /// The destination of a branch, JSR or JMP abs.
    Target,
    /// Any other address the instruction references (abs/zp, indexed, the pointer of an
    /// indirect mode, a JMP (ind) vector).
    Operand,
}

/// Spec 804 — one address the formatter printed: `line[start..end]` is the address text
/// (the line is ASCII, so byte offsets are character offsets), `addr` its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineSpan {
    pub start: usize,
    pub end: usize,
    pub addr: u16,
    pub role: SpanRole,
}

/// The address fields of one instruction: the control-transfer destination (JSR/JMP abs,
/// a branch) and any other referenced address. Zero page counts as an address.
fn operand_addresses(opcode: u8, mode: &str, addr: u16, size: u16, b1: u8, b2: u8) -> (Option<u16>, Option<u16>) {
    let abs = (b1 as u16) | ((b2 as u16) << 8);
    match mode {
        "rel" => {
            let signed = if b1 >= 0x80 { b1 as i32 - 0x100 } else { b1 as i32 };
            (Some(((addr as i32) + size as i32 + signed) as u16), None)
        }
        "abs" if opcode == 0x20 || opcode == 0x4c => (Some(abs), None),
        "abs" | "absx" | "absy" | "ind" => (None, Some(abs)),
        "zp" | "zpx" | "zpy" | "indx" | "indy" => (None, Some(b1 as u16)),
        _ => (None, None),
    }
}

/// 1:1 port of `disasmLine` (disasm6502.ts): `$addr  bb bb bb  MNEMONIC ops`.
/// Bytes padded to a fixed 8-char column; mnemonic upper-cased, operand hex
/// LOWER-case (VICE-ish). Returns (size, line).
pub fn disasm_line_ts(read: impl Fn(u16) -> u8, addr: u16) -> (u16, String) {
    let (size, line, _) = disasm_line_ts_spans(read, addr);
    (size, line)
}

/// [`disasm_line_ts`] plus the position of every address it printed (Spec 804). The
/// text is identical — the spans are what the formatter knows because it wrote them.
pub fn disasm_line_ts_spans(read: impl Fn(u16) -> u8, addr: u16) -> (u16, String, Vec<LineSpan>) {
    let opcode = read(addr);
    let (mne, mode) = mnemonic_mode_ts(opcode);
    let size = instr_len(opcode) as u16;
    let b1 = read(addr.wrapping_add(1));
    let b2 = read(addr.wrapping_add(2));
    // Operand text — operand hex LOWER-case, matching disasm6502.ts `hx`. The second
    // element is where, inside that text, the address sits.
    let (text, hex_at): (String, Option<(usize, usize)>) = match mode {
        "imp" | "acc" => (String::new(), None),
        "imm" => (format!("#${:02x}", b1), None),
        "zp" => (format!("${:02x}", b1), Some((0, 3))),
        "zpx" => (format!("${:02x},x", b1), Some((0, 3))),
        "zpy" => (format!("${:02x},y", b1), Some((0, 3))),
        "abs" => (format!("${:04x}", (b1 as u16) | ((b2 as u16) << 8)), Some((0, 5))),
        "absx" => (format!("${:04x},x", (b1 as u16) | ((b2 as u16) << 8)), Some((0, 5))),
        "absy" => (format!("${:04x},y", (b1 as u16) | ((b2 as u16) << 8)), Some((0, 5))),
        "ind" => (format!("(${:04x})", (b1 as u16) | ((b2 as u16) << 8)), Some((1, 6))),
        "indx" => (format!("(${:02x},x)", b1), Some((1, 4))),
        "indy" => (format!("(${:02x}),y", b1), Some((1, 4))),
        "rel" => {
            let signed = if b1 >= 0x80 { b1 as i32 - 0x100 } else { b1 as i32 };
            let target = ((addr as i32) + size as i32 + signed) as u16;
            (format!("${:04x}", target), Some((0, 5)))
        }
        _ => (String::new(), None),
    };
    // Bytes column: "bb bb bb" = 8 chars max; pad to 8 (disasm6502.ts padEnd(8)).
    let bytes: Vec<String> = (0..size).map(|i| format!("{:02x}", read(addr.wrapping_add(i)))).collect();
    let bytes_col = format!("{:<8}", bytes.join(" "));
    let head = format!("${:04x}  {}  {}", addr, bytes_col, mne.to_uppercase());
    let mut spans = vec![LineSpan { start: 0, end: 5, addr, role: SpanRole::Pc }];
    if text.is_empty() {
        return (size, head, spans);
    }
    if let Some((s, e)) = hex_at {
        let (value, role) = match operand_addresses(opcode, mode, addr, size, b1, b2) {
            (Some(t), _) => (t, SpanRole::Target),
            (None, Some(o)) => (o, SpanRole::Operand),
            (None, None) => unreachable!("every mode that prints an address has one"),
        };
        let base = head.len() + 1;
        spans.push(LineSpan { start: base + s, end: base + e, addr: value, role });
    }
    (size, format!("{head} {text}"), spans)
}

/// One decoded instruction in the `monitorDisasm` api/call shape.
#[derive(Clone, Debug)]
pub struct DisasmOne {
    pub addr: u16,
    pub bytes: Vec<u8>,
    pub mnemonic: String,
    pub operand: String,
    pub text: String,
    /// Spec 804 — the addressing mode (trx64-core table naming: imp/acc/imm/zp/…/rel).
    pub mode: &'static str,
    /// Spec 804 — a branch/JSR/JMP abs destination, as a number.
    pub target: Option<u16>,
    /// Spec 804 — any other address the instruction references, as a number.
    pub operand_addr: Option<u16>,
}

/// The daemon's `monitorDisasm` decoder, moved verbatim: UPPERCASE hex,
/// undocumented ops under their table kind (ISB/SBC_IMM…), `.byte $XX` for true
/// JAM holes. Wire-contract shape — see the module doc before changing anything.
pub fn disasm_one(addr: u16, read: impl Fn(u16) -> u8) -> DisasmOne {
    let opcode = read(addr);
    let len = instr_len(opcode);
    let bytes: Vec<u8> = (0..len as u16).map(|i| read(addr.wrapping_add(i))).collect();
    let b1 = bytes.get(1).copied().unwrap_or(0);
    let b2 = bytes.get(2).copied().unwrap_or(0);

    let (mne, mode) = MICROCODE_TABLE[opcode as usize]
        .map(|e| (e.op.to_uppercase(), e.mode))
        .or_else(|| UNDOC_TABLE[opcode as usize].map(|e| (e.kind.to_uppercase(), e.mode)))
        .unwrap_or_else(|| (format!(".byte ${:02X}", opcode), "imp"));

    let operand = match mode {
        "imp" | "acc" => String::new(),
        "imm" => format!("#${:02X}", b1),
        "zp" => format!("${:02X}", b1),
        "zpx" => format!("${:02X},X", b1),
        "zpy" => format!("${:02X},Y", b1),
        "rel" => {
            let off = b1 as i8 as i32;
            let target = (addr as i32 + 2 + off) as u16;
            format!("${:04X}", target)
        }
        "abs" => format!("${:04X}", (b1 as u16) | ((b2 as u16) << 8)),
        "absx" => format!("${:04X},X", (b1 as u16) | ((b2 as u16) << 8)),
        "absy" => format!("${:04X},Y", (b1 as u16) | ((b2 as u16) << 8)),
        "ind" => format!("(${:04X})", (b1 as u16) | ((b2 as u16) << 8)),
        "indx" => format!("(${:02X},X)", b1),
        "indy" => format!("(${:02X}),Y", b1),
        _ => String::new(),
    };

    let byte_str = bytes.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ");
    let text = if operand.is_empty() {
        format!("${:04X}  {:<8}  {}", addr, byte_str, mne)
    } else {
        format!("${:04X}  {:<8}  {} {}", addr, byte_str, mne, operand)
    };

    let (target, operand_addr) = operand_addresses(opcode, mode, addr, len as u16, b1, b2);
    DisasmOne { addr, bytes, mnemonic: mne, operand, text, mode, target, operand_addr }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf_read(addr0: u16, bytes: &'static [u8]) -> impl Fn(u16) -> u8 {
        move |a: u16| {
            let off = a.wrapping_sub(addr0) as usize;
            bytes.get(off).copied().unwrap_or(0)
        }
    }

    #[test]
    fn line_basic_modes() {
        // LDA #$0a
        let (size, line) = disasm_line_ts(buf_read(0xc000, &[0xa9, 0x0a]), 0xc000);
        assert_eq!(size, 2);
        assert_eq!(line, "$c000  a9 0a     LDA #$0a");
        // JMP ($c05a)
        let (size, line) = disasm_line_ts(buf_read(0xc000, &[0x6c, 0x5a, 0xc0]), 0xc000);
        assert_eq!(size, 3);
        assert_eq!(line, "$c000  6c 5a c0  JMP ($c05a)");
        // BNE backwards across the top of memory
        let (size, line) = disasm_line_ts(buf_read(0x0002, &[0xd0, 0xfa]), 0x0002);
        assert_eq!(size, 2);
        assert_eq!(line, "$0002  d0 fa     BNE $fffe");
    }

    #[test]
    fn line_undoc_naming_matches_ts_oracle() {
        // $e3 = isb in trx64-core tables, but the TS oracle prints ISC.
        let (_, line) = disasm_line_ts(buf_read(0xc000, &[0xe3, 0x10]), 0xc000);
        assert_eq!(line, "$c000  e3 10     ISC ($10,x)");
        // $eb = sbc_imm in the tables → SBC in the oracle.
        let (_, line) = disasm_line_ts(buf_read(0xc000, &[0xeb, 0x01]), 0xc000);
        assert_eq!(line, "$c000  eb 01     SBC #$01");
        // $02 = hole in both tables → JAM, size 1.
        let (size, line) = disasm_line_ts(buf_read(0xc000, &[0x02]), 0xc000);
        assert_eq!(size, 1);
        assert_eq!(line, "$c000  02        JAM");
    }

    #[test]
    fn spans_cut_out_exactly_the_printed_addresses() {
        // JSR: the own address (pc) and the destination (target).
        let (_, line, spans) = disasm_line_ts_spans(buf_read(0xc000, &[0x20, 0xd2, 0xff]), 0xc000);
        assert_eq!(line, "$c000  20 d2 ff  JSR $ffd2");
        assert_eq!(spans.len(), 2);
        assert_eq!(&line[spans[0].start..spans[0].end], "$c000");
        assert_eq!((spans[0].addr, spans[0].role), (0xc000, SpanRole::Pc));
        assert_eq!(&line[spans[1].start..spans[1].end], "$ffd2");
        assert_eq!((spans[1].addr, spans[1].role), (0xffd2, SpanRole::Target));
        // A branch: the resolved destination.
        let (_, line, spans) = disasm_line_ts_spans(buf_read(0x0002, &[0xd0, 0xfa]), 0x0002);
        assert_eq!(&line[spans[1].start..spans[1].end], "$fffe");
        assert_eq!((spans[1].addr, spans[1].role), (0xfffe, SpanRole::Target));
        // Indirect: the pointer is an operand, inside the parentheses.
        let (_, line, spans) = disasm_line_ts_spans(buf_read(0xc000, &[0x6c, 0x5a, 0xc0]), 0xc000);
        assert_eq!(&line[spans[1].start..spans[1].end], "$c05a");
        assert_eq!(spans[1].role, SpanRole::Operand);
        let (_, line, spans) = disasm_line_ts_spans(buf_read(0xc000, &[0xb1, 0xfb]), 0xc000);
        assert_eq!(&line[spans[1].start..spans[1].end], "$fb");
        assert_eq!((spans[1].addr, spans[1].role), (0x00fb, SpanRole::Operand));
        // Immediate and implied print no operand address.
        let (_, _, spans) = disasm_line_ts_spans(buf_read(0xc000, &[0xa9, 0x0a]), 0xc000);
        assert_eq!(spans.len(), 1);
        let (_, _, spans) = disasm_line_ts_spans(buf_read(0xc000, &[0xea]), 0xc000);
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn the_spans_line_is_the_plain_line_for_every_opcode() {
        for op in 0u8..=255 {
            let bytes = [op, 0x34, 0x12];
            let read = |a: u16| bytes.get(a.wrapping_sub(0x1000) as usize).copied().unwrap_or(0);
            let (s1, l1) = disasm_line_ts(read, 0x1000);
            let (s2, l2, spans) = disasm_line_ts_spans(read, 0x1000);
            assert_eq!((s1, &l1), (s2, &l2), "opcode {op:02x}");
            for sp in &spans {
                let hex = &l2[sp.start..sp.end];
                assert!(hex.starts_with('$'), "opcode {op:02x}: span {hex:?}");
                assert_eq!(u16::from_str_radix(&hex[1..], 16).unwrap(), sp.addr, "opcode {op:02x}");
            }
        }
    }

    #[test]
    fn one_carries_its_addresses_as_numbers() {
        let d = disasm_one(0xc000, buf_read(0xc000, &[0x20, 0xd2, 0xff]));
        assert_eq!((d.mode, d.target, d.operand_addr), ("abs", Some(0xffd2), None));
        let d = disasm_one(0xc000, buf_read(0xc000, &[0xad, 0x20, 0xd0]));
        assert_eq!((d.target, d.operand_addr), (None, Some(0xd020)));
        let d = disasm_one(0xc000, buf_read(0xc000, &[0xf0, 0x02]));
        assert_eq!((d.target, d.operand_addr), (Some(0xc004), None));
        let d = disasm_one(0xc000, buf_read(0xc000, &[0xa9, 0x02]));
        assert_eq!((d.target, d.operand_addr), (None, None));
    }

    #[test]
    fn one_keeps_daemon_wire_shape() {
        // JAM hole keeps the `.byte $XX` fallback (monitorDisasm wire contract).
        let d = disasm_one(0xc000, buf_read(0xc000, &[0x02]));
        assert_eq!(d.mnemonic, ".byte $02");
        assert_eq!(d.text, "$C000  02        .byte $02");
        // Undoc stays under its table kind (ISB, not ISC) on this surface.
        let d = disasm_one(0xc000, buf_read(0xc000, &[0xe7, 0x10]));
        assert_eq!(d.mnemonic, "ISB");
        assert_eq!(d.text, "$C000  E7 10     ISB $10");
    }
}
