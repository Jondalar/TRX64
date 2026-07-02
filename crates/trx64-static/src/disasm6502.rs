//! 6502/6510 static disassembler — extracted from trx64-daemon `main.rs`
//! (capability-cut migration step 1) so every consumer shares one decoder.
//!
//! Two formatters, two contracts — do NOT "unify" them:
//!
//!  - [`disasm_line_ts`] — 1:1 port of the C64RE TS oracle `disasm6502.ts`
//!    `disasmLine`: `$addr  bb bb bb  MNEMONIC ops`, bytes padded to a fixed
//!    8-char column, mnemonic upper-cased, operand hex LOWER-case (VICE-ish).
//!    Golden-tested byte-identical vs the TS oracle (`tests/goldens/`).
//!  - [`disasm_one`] — the `monitorDisasm` api/call shape (UPPERCASE hex,
//!    `.byte $XX` fallback for true JAM holes). This is the daemon's MCP wire
//!    contract, moved verbatim; its text convention intentionally differs.

use std::collections::BTreeMap;

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

/// 1:1 port of `disasmLine` (disasm6502.ts): `$addr  bb bb bb  MNEMONIC ops`.
/// Bytes padded to a fixed 8-char column; mnemonic upper-cased, operand hex
/// LOWER-case (VICE-ish). Returns (size, line).
pub fn disasm_line_ts(read: impl Fn(u16) -> u8, addr: u16) -> (u16, String) {
    let opcode = read(addr);
    let (mne, mode) = mnemonic_mode_ts(opcode);
    let size = instr_len(opcode) as u16;
    let b1 = read(addr.wrapping_add(1));
    let b2 = read(addr.wrapping_add(2));
    // Operand text — operand hex LOWER-case, matching disasm6502.ts `hx`.
    let text = match mode {
        "imp" | "acc" => String::new(),
        "imm" => format!("#${:02x}", b1),
        "zp" => format!("${:02x}", b1),
        "zpx" => format!("${:02x},x", b1),
        "zpy" => format!("${:02x},y", b1),
        "abs" => format!("${:04x}", (b1 as u16) | ((b2 as u16) << 8)),
        "absx" => format!("${:04x},x", (b1 as u16) | ((b2 as u16) << 8)),
        "absy" => format!("${:04x},y", (b1 as u16) | ((b2 as u16) << 8)),
        "ind" => format!("(${:04x})", (b1 as u16) | ((b2 as u16) << 8)),
        "indx" => format!("(${:02x},x)", b1),
        "indy" => format!("(${:02x}),y", b1),
        "rel" => {
            let signed = if b1 >= 0x80 { b1 as i32 - 0x100 } else { b1 as i32 };
            let target = ((addr as i32) + size as i32 + signed) as u16;
            format!("${:04x}", target)
        }
        _ => String::new(),
    };
    // Bytes column: "bb bb bb" = 8 chars max; pad to 8 (disasm6502.ts padEnd(8)).
    let bytes: Vec<String> = (0..size).map(|i| format!("{:02x}", read(addr.wrapping_add(i)))).collect();
    let bytes_col = format!("{:<8}", bytes.join(" "));
    let ops = if text.is_empty() { String::new() } else { format!(" {text}") };
    let line = format!("${:04x}  {}  {}{}", addr, bytes_col, mne.to_uppercase(), ops);
    (size, line)
}

/// `disasmLine` WITH the Spec 754 §3.3f (Block F) label annotation
/// (disasm6502.ts:155-161): a target-address label is appended as `; → name`, and
/// the instruction's OWN address label is prepended as an asm-style `name:` line.
/// Both the label AND the numeric address stay visible. Mirrors the TS
/// `di.target ?? (di.size === 3 ? di.operand : undefined)` target resolution.
pub fn disasm_line_ts_labeled(
    read: impl Fn(u16) -> u8,
    addr: u16,
    labels: &BTreeMap<u16, String>,
) -> (u16, String) {
    let (size, mut line) = disasm_line_ts(&read, addr);
    let opcode = read(addr);
    let b1 = read(addr.wrapping_add(1));
    let b2 = read(addr.wrapping_add(2));
    let (_, mode) = mnemonic_mode_ts(opcode);
    // di.target (abs / rel) ?? (di.size === 3 ? di.operand : undefined).
    let target: Option<u16> = match mode {
        "abs" => Some((b1 as u16) | ((b2 as u16) << 8)),
        "rel" => {
            let signed = if b1 >= 0x80 { b1 as i32 - 0x100 } else { b1 as i32 };
            Some(((addr as i32) + size as i32 + signed) as u16)
        }
        _ if size == 3 => Some((b1 as u16) | ((b2 as u16) << 8)),
        _ => None,
    };
    if let Some(t) = target {
        if let Some(name) = labels.get(&t) {
            line.push_str(&format!("   ; → {name}"));
        }
    }
    if let Some(own) = labels.get(&addr) {
        line = format!("{own}:\n{line}");
    }
    (size, line)
}

/// One decoded instruction in the `monitorDisasm` api/call shape.
#[derive(Clone, Debug)]
pub struct DisasmOne {
    pub addr: u16,
    pub bytes: Vec<u8>,
    pub mnemonic: String,
    pub operand: String,
    pub text: String,
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

    DisasmOne { addr, bytes, mnemonic: mne, operand, text }
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
    fn labeled_line() {
        let mut labels = BTreeMap::new();
        labels.insert(0xc000_u16, "entry".to_string());
        labels.insert(0xffd2_u16, "CHROUT".to_string());
        let (size, line) =
            disasm_line_ts_labeled(buf_read(0xc000, &[0x20, 0xd2, 0xff]), 0xc000, &labels);
        assert_eq!(size, 3);
        assert_eq!(line, "entry:\n$c000  20 d2 ff  JSR $ffd2   ; → CHROUT");
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
