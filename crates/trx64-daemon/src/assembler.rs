//! One-line 6502/6510 assembler for the monitor's inline `a` command.
//!
//! 1:1 port of c64re `src/runtime/headless/debug/assembler6502.ts` (Spec 754
//! §3.3c). Assemble a single instruction (no address prefix) at a given PC,
//! producing the encoded bytes (opcode + little-endian operands).
//!
//! SINGLE SOURCE OF TRUTH: the opcode table is the REVERSE index of the runtime
//! disassembler — here `trx64_core::tables::MICROCODE_TABLE` (= TS reversing
//! `disasm6502`). We walk all 256 documented opcodes once to build a
//! (mnemonic|mode → opcode) map, so assemble→disasm round-trips exactly for every
//! documented opcode (the monitor's `d` and `a` MUST agree, or the user types what
//! they just saw and gets different bytes).
//!
//! MODE-NAME NOTE: the TS disassembler uses mode strings like `impl`/`zp,x`/
//! `(zp,x)`; TRX64's table uses `imp`/`zpx`/`indx`. This port indexes against
//! TRX64's OWN mode strings (so it agrees with TRX64's `d`), and the folding logic
//! (immediate / branch / indirect / zp-vs-abs) is line-for-line the TS algorithm.

use std::collections::HashMap;
use std::sync::OnceLock;
use trx64_core::tables::MICROCODE_TABLE;

/// Success: encoded instruction bytes (opcode first) + total instruction size.
#[derive(Debug, Clone)]
pub struct AssembleOk {
    pub bytes: Vec<u8>,
    pub size: u16,
}

/// Documented NMOS 6502/6510 mnemonics (= TS `DOCUMENTED_MNEMONICS`). The
/// undocumented set (slo/rla/sre/… and the undocumented nop/sbc aliases) is
/// excluded from the assemble index — v1 emits only the documented set.
const DOCUMENTED_MNEMONICS: &[&str] = &[
    // load/store
    "lda", "ldx", "ldy", "sta", "stx", "sty",
    // transfers
    "tax", "tay", "txa", "tya", "tsx", "txs",
    // stack
    "pha", "php", "pla", "plp",
    // logic
    "and", "ora", "eor", "bit",
    // arithmetic
    "adc", "sbc", "cmp", "cpx", "cpy",
    // inc/dec
    "inc", "dec", "inx", "iny", "dex", "dey",
    // shifts
    "asl", "lsr", "rol", "ror",
    // jumps/calls
    "jmp", "jsr", "rts", "rti",
    // branches
    "bcc", "bcs", "beq", "bne", "bmi", "bpl", "bvc", "bvs",
    // flags
    "clc", "sec", "cld", "sed", "cli", "sei", "clv",
    // misc
    "brk", "nop",
];

fn is_documented(m: &str) -> bool {
    DOCUMENTED_MNEMONICS.contains(&m)
}

/// Reverse index `(mnemonic|mode) → opcode`, built once from `MICROCODE_TABLE`.
/// First (lowest) opcode for each (mnemonic,mode) wins — deterministic. (The TS
/// `CANONICAL` `nop|impl→0xea` override is unneeded here: `MICROCODE_TABLE` lists
/// only the documented `nop` at 0xea — the undocumented nop variants live in
/// `UNDOC_TABLE`, which this index never walks.)
fn reverse() -> &'static HashMap<String, u8> {
    static REV: OnceLock<HashMap<String, u8>> = OnceLock::new();
    REV.get_or_init(|| {
        let mut m: HashMap<String, u8> = HashMap::new();
        for op in 0u16..=0xff {
            if let Some(e) = MICROCODE_TABLE[op as usize] {
                if !is_documented(e.op) {
                    continue;
                }
                let key = format!("{}|{}", e.op, e.mode);
                m.entry(key).or_insert(op as u8);
            }
        }
        m
    })
}

fn lookup(mnemonic: &str, mode: &str) -> Option<u8> {
    reverse().get(&format!("{mnemonic}|{mode}")).copied()
}

fn has_mode(mnemonic: &str, mode: &str) -> bool {
    reverse().contains_key(&format!("{mnemonic}|{mode}"))
}

/// A parsed numeric operand. `forced_wide` => the literal was written "wide"
/// (>=3 hex digits / 16-bit decimal / value>0xff) → caller must NOT fold to zp.
struct ParsedValue {
    value: i32,
    forced_wide: bool,
}

fn parse_value(raw: &str) -> Result<ParsedValue, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("missing operand".into());
    }
    let bytes = t.as_bytes();
    // Binary: %1010
    if bytes[0] == b'%' {
        let digits = &t[1..];
        if digits.is_empty() || digits.bytes().any(|b| b != b'0' && b != b'1') {
            return Err(format!("bad binary operand '{raw}'"));
        }
        let value = i32::from_str_radix(digits, 2).map_err(|_| format!("bad binary operand '{raw}'"))?;
        return Ok(ParsedValue { value, forced_wide: digits.len() > 8 || value > 0xff });
    }
    // Hex: $xx
    if bytes[0] == b'$' {
        let digits = &t[1..];
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("bad hex operand '{raw}'"));
        }
        let value = i32::from_str_radix(digits, 16).map_err(|_| format!("bad hex operand '{raw}'"))?;
        return Ok(ParsedValue { value, forced_wide: digits.len() > 2 || value > 0xff });
    }
    // Pure decimal: only digits 0-9
    if t.bytes().all(|b| b.is_ascii_digit()) {
        let value = t.parse::<i32>().map_err(|_| format!("bad operand '{raw}'"))?;
        return Ok(ParsedValue { value, forced_wide: value > 0xff });
    }
    // Bare hex token (a-f, no other junk): `lda #ff`, `jmp c000`
    if t.bytes().all(|b| b.is_ascii_hexdigit()) {
        let value = i32::from_str_radix(t, 16).map_err(|_| format!("bad operand '{raw}'"))?;
        return Ok(ParsedValue { value, forced_wide: t.len() > 2 || value > 0xff });
    }
    Err(format!("bad operand '{raw}'"))
}

fn byte_ok(opcode: u8) -> AssembleOk {
    AssembleOk { bytes: vec![opcode], size: 1 }
}
fn byte2_ok(opcode: u8, operand: i32) -> AssembleOk {
    AssembleOk { bytes: vec![opcode, (operand & 0xff) as u8], size: 2 }
}
fn byte3_ok(opcode: u8, operand: i32) -> AssembleOk {
    AssembleOk {
        bytes: vec![opcode, (operand & 0xff) as u8, ((operand >> 8) & 0xff) as u8],
        size: 3,
    }
}

/// Assemble one 6502/6510 instruction (no address prefix). `pc` is where the
/// instruction will live (for branch offset computation). Returns the encoded
/// bytes + size, or an `Err(reason)`. 1:1 with `assembleLine` (assembler6502.ts).
pub fn assemble_line(text: &str, pc: u16) -> Result<AssembleOk, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("empty instruction".into());
    }
    // Split mnemonic (exactly 3 letters) from operand on the first whitespace run.
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() < 3 || !chars[..3].iter().all(|c| c.is_ascii_alphabetic()) {
        return Err(format!("unparsable instruction '{}'", trimmed));
    }
    // A 4th char must be a word boundary (whitespace) — `ldaa` is not `lda a`.
    if chars.len() > 3 && !chars[3].is_whitespace() {
        return Err(format!("unparsable instruction '{}'", trimmed));
    }
    let mnemonic: String = chars[..3].iter().collect::<String>().to_ascii_lowercase();
    // Strip ALL internal whitespace from the operand (`lda $05 , x` == `lda $05,x`).
    let operand_text: String = chars[3..].iter().filter(|c| !c.is_whitespace()).collect();

    if !is_documented(&mnemonic) {
        return Err(format!("unknown mnemonic '{mnemonic}'"));
    }

    // No operand: implied or accumulator.
    if operand_text.is_empty() || operand_text.eq_ignore_ascii_case("a") {
        if let Some(op) = lookup(&mnemonic, "imp").or_else(|| lookup(&mnemonic, "acc")) {
            return Ok(byte_ok(op));
        }
        return Err(format!("'{mnemonic}' requires an operand"));
    }

    let lower = operand_text.to_ascii_lowercase();
    let lbytes = lower.as_bytes();

    // Relative branches: operand is a TARGET ADDRESS; encode the signed offset.
    if has_mode(&mnemonic, "rel") {
        let pv = parse_value(&operand_text)?;
        if pv.value < 0 || pv.value > 0xffff {
            return Err(format!("branch target ${:x} out of range", pv.value));
        }
        let op = lookup(&mnemonic, "rel").unwrap();
        let offset = pv.value - (((pc as i32) + 2) & 0xffff);
        if offset < -128 || offset > 127 {
            return Err(format!("branch out of range ({offset} bytes)"));
        }
        return Ok(byte2_ok(op, offset & 0xff));
    }

    // Immediate: #$xx / #dd / #ff
    if lbytes[0] == b'#' {
        let pv = parse_value(&operand_text[1..])?;
        if pv.value > 0xff {
            return Err(format!("immediate value ${:x} overflows a byte", pv.value));
        }
        let op = lookup(&mnemonic, "imm").ok_or_else(|| format!("'{mnemonic}' has no immediate mode"))?;
        return Ok(byte2_ok(op, pv.value));
    }

    // Indirect family: starts with '('.
    if lbytes[0] == b'(' {
        // (zp,x):  ( <val> , x )
        if let Some(inner) = strip_wrap(&lower, "(", ",x)") {
            let pv = parse_value(inner)?;
            if pv.value > 0xff {
                return Err(format!("(zp,x) operand ${:x} not zero-page", pv.value));
            }
            let op = lookup(&mnemonic, "indx").ok_or_else(|| format!("'{mnemonic}' has no (zp,x) mode"))?;
            return Ok(byte2_ok(op, pv.value));
        }
        // (zp),y:  ( <val> ) , y
        if let Some(inner) = strip_wrap(&lower, "(", "),y") {
            let pv = parse_value(inner)?;
            if pv.value > 0xff {
                return Err(format!("(zp),y operand ${:x} not zero-page", pv.value));
            }
            let op = lookup(&mnemonic, "indy").ok_or_else(|| format!("'{mnemonic}' has no (zp),y mode"))?;
            return Ok(byte2_ok(op, pv.value));
        }
        // indirect:  ( <val> )    — JMP only
        if let Some(inner) = strip_wrap(&lower, "(", ")") {
            if !inner.contains(',') {
                let pv = parse_value(inner)?;
                if pv.value > 0xffff {
                    return Err(format!("indirect operand ${:x} overflows 16 bits", pv.value));
                }
                let op = lookup(&mnemonic, "ind").ok_or_else(|| format!("'{mnemonic}' has no indirect mode"))?;
                return Ok(byte3_ok(op, pv.value));
            }
        }
        return Err(format!("bad indirect operand '{operand_text}'"));
    }

    // Indexed / plain: <val> | <val>,x | <val>,y
    let (suffix, value_part): (&str, &str) = if let Some(p) = lower.strip_suffix(",x") {
        (",x", p)
    } else if let Some(p) = lower.strip_suffix(",y") {
        (",y", p)
    } else if lower.contains(',') {
        return Err(format!("bad index suffix in '{operand_text}'"));
    } else {
        ("", lower.as_str())
    };

    let pv = parse_value(value_part)?;
    if pv.value < 0 || pv.value > 0xffff {
        return Err(format!("operand ${:x} overflows 16 bits", pv.value));
    }

    let fits_zp = pv.value <= 0xff && !pv.forced_wide;

    // Candidate modes by suffix (TRX64 mode names).
    let (zp_mode, abs_mode) = match suffix {
        ",x" => ("zpx", "absx"),
        ",y" => ("zpy", "absy"),
        _ => ("zp", "abs"),
    };

    if fits_zp {
        if let Some(zp_op) = lookup(&mnemonic, zp_mode) {
            return Ok(byte2_ok(zp_op, pv.value));
        }
        // No zp form (e.g. only abs exists) — fall through to absolute.
    }

    if let Some(abs_op) = lookup(&mnemonic, abs_mode) {
        return Ok(byte3_ok(abs_op, pv.value));
    }

    // Neither mode exists for this mnemonic — report what was attempted (= TS
    // wording with TRX64's abs mode name).
    let tried_zp = if fits_zp { format!("{zp_mode} or ") } else { String::new() };
    let any = [
        "imp", "acc", "imm", "zp", "zpx", "zpy", "abs", "absx", "absy", "ind", "indx", "indy", "rel",
    ]
    .iter()
    .any(|m| has_mode(&mnemonic, m));
    if !any {
        return Err(format!("unknown mnemonic '{mnemonic}'"));
    }
    Err(format!("'{mnemonic}' has no {tried_zp}{abs_mode} mode"))
}

/// `( <inner> )`-style strip: returns the inner text when `s` begins with `pre`
/// and ends with `suf` (and there is room between them), else None.
fn strip_wrap<'a>(s: &'a str, pre: &str, suf: &str) -> Option<&'a str> {
    if s.len() >= pre.len() + suf.len() && s.starts_with(pre) && s.ends_with(suf) {
        Some(&s[pre.len()..s.len() - suf.len()])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(text: &str, pc: u16) -> Vec<u8> {
        assemble_line(text, pc).unwrap().bytes
    }

    #[test]
    fn immediate_and_zp_and_abs() {
        assert_eq!(bytes("lda #$01", 0xc000), vec![0xa9, 0x01]);
        assert_eq!(bytes("sta $d020", 0xc010), vec![0x8d, 0x20, 0xd0]);
        assert_eq!(bytes("lda $fb", 0xc000), vec![0xa5, 0xfb]); // zp fold
        assert_eq!(bytes("lda $00fb", 0xc000), vec![0xad, 0xfb, 0x00]); // forced wide → abs
    }

    #[test]
    fn implied_jsr_rts_branch() {
        assert_eq!(bytes("rts", 0xc030), vec![0x60]);
        assert_eq!(bytes("jsr $fce2", 0xc020), vec![0x20, 0xe2, 0xfc]);
        assert_eq!(bytes("nop", 0xc000), vec![0xea]);
        assert_eq!(bytes("asl", 0xc000), vec![0x0a]); // accumulator
        // BEQ from $c000 to $c010: offset = 0x10 - (0xc000+2) ... target-(pc+2)=0x0e
        assert_eq!(bytes("beq $c010", 0xc000), vec![0xf0, 0x0e]);
    }

    #[test]
    fn indirect_family() {
        assert_eq!(bytes("sta ($fd),y", 0xc000), vec![0x91, 0xfd]);
        assert_eq!(bytes("lda ($20,x)", 0xc000), vec![0xa1, 0x20]);
        assert_eq!(bytes("jmp ($fffc)", 0xc000), vec![0x6c, 0xfc, 0xff]);
    }

    #[test]
    fn errors() {
        assert!(assemble_line("lda #$1234", 0xc000).is_err()); // imm overflow
        assert!(assemble_line("foo $00", 0xc000).is_err()); // unknown mnemonic
        assert!(assemble_line("beq $d000", 0xc000).is_err()); // branch out of range
    }
}
