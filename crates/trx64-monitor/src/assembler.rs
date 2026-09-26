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

// ─────────────────────────────────────────────────────────────────────────────
// Spec 809 §5 — source in, bytes out.
//
// `assemble_line` takes one instruction. A patch is a handful of them with at least one
// label, and typing those one at a time through `a` is not a loop anybody runs twice. So
// this is the small door above it: a block of source, two passes, bytes and a load
// address out — exactly what `sandbox/run` takes as a patch.
//
// Every instruction still goes through `assemble_line`, so `d` and `a` keep agreeing;
// what this adds is only what a single line cannot have: labels, `*`, `<`/`>`, `+`/`-`,
// `.byte`/`.word`, and one `*=`/`.org` before the first byte. Explicitly NOT a build
// system: no includes, no macros, no strings, no second segment. A patch that needs
// those is a `.prg`, and `bload` exists.
//
// Sizing. A label used before it is defined has no value in pass 1, so that line is
// sized as if the value were wide (absolute), and pass 2 keeps it wide even when the
// label turns out to be zero-page — otherwise the line would shrink between the passes
// and every later address would move. A label defined BEFORE its use is known in pass 1
// and may fold to zero-page. This is the classic two-pass rule, stated because it is
// the one place the output depends on the order of the source.
// ─────────────────────────────────────────────────────────────────────────────

/// One assembled block: the bytes from `origin` on, the labels it defined, and every
/// error with its 1-based source line. `bytes` is empty whenever `errors` is not.
#[derive(Debug, Clone, Default)]
pub struct AssembledBlock {
    pub origin: u16,
    pub bytes: Vec<u8>,
    pub labels: std::collections::BTreeMap<String, u16>,
    pub errors: Vec<(usize, String)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Branch,
    Immediate,
    Other,
}

/// Evaluate `expr` — terms joined by `+`/`-`, each a number, a label or `*` (the current
/// address), the whole optionally prefixed by `<` (low byte) or `>` (high byte).
/// `unknown` collects labels with no value yet; they evaluate to a placeholder chosen by
/// context so pass 1 can size the line.
fn eval_expr(
    expr: &str,
    pc: u16,
    labels: &HashMap<String, u16>,
    ctx: Ctx,
    unknown: &mut Vec<String>,
) -> Result<i32, String> {
    let e = expr.trim();
    let (sel, body) = if let Some(r) = e.strip_prefix('<') {
        (Some('<'), r)
    } else if let Some(r) = e.strip_prefix('>') {
        (Some('>'), r)
    } else {
        (None, e)
    };
    let mut total: i32 = 0;
    let mut sign = 1;
    let mut term = String::new();
    let flush = |term: &mut String, sign: i32, total: &mut i32, unknown: &mut Vec<String>| -> Result<(), String> {
        let t = term.trim().to_string();
        term.clear();
        if t.is_empty() {
            return Err(format!("missing term in '{expr}'"));
        }
        let v = if t == "*" {
            pc as i32
        } else if let Ok(pv) = parse_value(&t) {
            pv.value
        } else if t.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            match labels.get(&t) {
                Some(&v) => v as i32,
                None => {
                    unknown.push(t.clone());
                    match ctx {
                        Ctx::Branch => pc as i32,
                        Ctx::Immediate => 0,
                        Ctx::Other => 0xffff,
                    }
                }
            }
        } else {
            return Err(format!("bad term '{t}'"));
        };
        *total += sign * v;
        Ok(())
    };
    // A leading `*` is the current address, not an operator.
    for (i, c) in body.char_indices() {
        if (c == '+' || c == '-') && i > 0 {
            flush(&mut term, sign, &mut total, unknown)?;
            sign = if c == '+' { 1 } else { -1 };
        } else {
            term.push(c);
        }
    }
    flush(&mut term, sign, &mut total, unknown)?;
    let v = total & 0xffff;
    Ok(match sel {
        Some('<') => v & 0xff,
        Some('>') => (v >> 8) & 0xff,
        _ => total,
    })
}

/// Split an operand into (prefix, expression, suffix) along the addressing syntax
/// `assemble_line` understands: `#e`, `(e,x)`, `(e),y`, `(e)`, `e,x`, `e,y`, `e`.
fn split_operand(op: &str) -> (&'static str, String, &'static str) {
    let lower = op.to_ascii_lowercase();
    if let Some(r) = op.strip_prefix('#') {
        return ("#", r.to_string(), "");
    }
    if lower.starts_with('(') {
        if lower.ends_with(",x)") {
            return ("(", op[1..op.len() - 3].to_string(), ",x)");
        }
        if lower.ends_with("),y") {
            return ("(", op[1..op.len() - 3].to_string(), "),y");
        }
        if lower.ends_with(')') {
            return ("(", op[1..op.len() - 1].to_string(), ")");
        }
    }
    if lower.ends_with(",x") {
        return ("", op[..op.len() - 2].to_string(), ",x");
    }
    if lower.ends_with(",y") {
        return ("", op[..op.len() - 2].to_string(), ",y");
    }
    ("", op.to_string(), "")
}

/// Resolve the operand of one instruction to the literal text `assemble_line` takes.
/// A plain literal passes through untouched, so `lda $00fb` stays wide exactly as it
/// does through `a`. `wide` forces the absolute form (see the module note on sizing).
fn resolve_operand(
    mnemonic: &str,
    operand: &str,
    pc: u16,
    labels: &HashMap<String, u16>,
    wide: bool,
    unknown: &mut Vec<String>,
) -> Result<String, String> {
    let compact: String = operand.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() || compact.eq_ignore_ascii_case("a") {
        return Ok(compact);
    }
    let (pre, expr, suf) = split_operand(&compact);
    if parse_value(&expr).is_ok() {
        return Ok(compact);
    }
    let ctx = if has_mode(mnemonic, "rel") {
        Ctx::Branch
    } else if pre == "#" {
        Ctx::Immediate
    } else {
        Ctx::Other
    };
    let before = unknown.len();
    let v = eval_expr(&expr, pc, labels, ctx, unknown)?;
    let this_line_unknown = unknown.len() > before;
    // A placeholder plus an offset can leave 16 bits (`ptr+1` with `ptr` unknown is
    // $FFFF+1). Pass 1 only sizes the line, so wrap it; pass 2 has the real value, and an
    // unresolved label there is reported as undefined, not as out of range.
    let v = if this_line_unknown { v & 0xffff } else { v };
    if !(0..=0xffff).contains(&v) {
        return Err(format!("value {v} out of range in '{operand}'"));
    }
    let text = if pre == "#" || (v <= 0xff && !wide && !this_line_unknown && ctx != Ctx::Branch) {
        format!("${v:02x}")
    } else {
        format!("${v:04x}")
    };
    Ok(format!("{pre}{text}{suf}"))
}

enum Stmt {
    Org(String),
    Byte(Vec<String>),
    Word(Vec<String>),
    Insn(String, String),
}

/// One parsed source line: an optional label, an optional statement, and — instead of
/// both — an optional `name = value` constant.
type ParsedLine = (Option<String>, Option<Stmt>, Option<(String, String)>);

/// Parse one source line. Comments start at `;`.
fn parse_line(raw: &str) -> Result<ParsedLine, String> {
    let line = raw.split(';').next().unwrap_or("").trim();
    if line.is_empty() {
        return Ok((None, None, None));
    }
    // `*= e` / `* = e`
    if let Some(r) = line.strip_prefix('*') {
        if let Some(e) = r.trim_start().strip_prefix('=') {
            return Ok((None, Some(Stmt::Org(e.trim().to_string())), None));
        }
    }
    // `name = e` — a constant.
    if let Some((l, r)) = line.split_once('=') {
        let name = l.trim();
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Ok((None, None, Some((name.to_string(), r.trim().to_string()))));
        }
    }
    let (label, rest) = match line.split_once(':') {
        Some((l, r)) if !l.trim().is_empty() && l.trim().chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
            (Some(l.trim().to_string()), r.trim())
        }
        _ => (None, line),
    };
    if rest.is_empty() {
        return Ok((label, None, None));
    }
    let (head, tail) = match rest.split_once(char::is_whitespace) {
        Some((h, t)) => (h, t.trim()),
        None => (rest, ""),
    };
    let list = |t: &str| -> Vec<String> { t.split(',').map(|x| x.trim().to_string()).collect() };
    let stmt = match head.to_ascii_lowercase().as_str() {
        ".org" => Stmt::Org(tail.to_string()),
        ".byte" | ".by" | ".db" => Stmt::Byte(list(tail)),
        ".word" | ".wo" | ".dw" => Stmt::Word(list(tail)),
        h if h.starts_with('.') => return Err(format!("unsupported directive '{head}' — this is not a build system")),
        _ => Stmt::Insn(head.to_string(), tail.to_string()),
    };
    Ok((label, Some(stmt), None))
}

/// Assemble a block of 6502 source at `origin` (a `*=`/`.org` before the first byte
/// overrides it). Two passes; see the module note for how forward references are sized.
pub fn assemble_block(source: &str, origin: u16) -> AssembledBlock {
    let lines: Vec<&str> = source.lines().collect();
    let mut labels: HashMap<String, u16> = HashMap::new();
    let mut wide_lines: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut out = AssembledBlock { origin, ..Default::default() };

    for pass in 1..=2 {
        let mut pc = out.origin;
        let mut org = out.origin;
        let mut emitted = false;
        let mut bytes: Vec<u8> = vec![];
        let mut errors: Vec<(usize, String)> = vec![];
        for (idx, raw) in lines.iter().enumerate() {
            let n = idx + 1;
            let parsed = match parse_line(raw) {
                Ok(p) => p,
                Err(e) => {
                    errors.push((n, e));
                    continue;
                }
            };
            let (label, stmt, constant) = parsed;
            if let Some((name, expr)) = constant {
                let mut unk = vec![];
                match eval_expr(&expr, pc, &labels, Ctx::Other, &mut unk) {
                    Ok(v) if unk.is_empty() => {
                        labels.insert(name, (v & 0xffff) as u16);
                    }
                    Ok(_) if pass == 1 => {}
                    Ok(_) => errors.push((n, format!("'{name}' uses an undefined label: {}", unk.join(", ")))),
                    Err(e) => errors.push((n, e)),
                }
                continue;
            }
            if let Some(l) = label {
                if pass == 1 && labels.contains_key(&l) {
                    errors.push((n, format!("label '{l}' defined twice")));
                }
                labels.insert(l, pc);
            }
            let Some(stmt) = stmt else { continue };
            let mut unk: Vec<String> = vec![];
            match stmt {
                Stmt::Org(e) => {
                    if emitted {
                        errors.push((n, "a second *= would start a second segment — one block is one contiguous patch".into()));
                        continue;
                    }
                    match eval_expr(&e, pc, &labels, Ctx::Other, &mut unk) {
                        Ok(v) if unk.is_empty() && (0..=0xffff).contains(&v) => {
                            pc = v as u16;
                            org = pc;
                        }
                        Ok(_) => errors.push((n, format!("*= needs a value known where it stands: '{e}'"))),
                        Err(err) => errors.push((n, err)),
                    }
                }
                Stmt::Byte(items) | Stmt::Word(items) if items.iter().any(|x| x.is_empty()) => {
                    errors.push((n, "empty item in a data list".into()));
                }
                Stmt::Byte(items) => {
                    for it in items {
                        match eval_expr(&it, pc, &labels, Ctx::Immediate, &mut unk) {
                            Ok(v) if pass == 2 && !(0..=0xff).contains(&v) => errors.push((n, format!(".byte value {v} overflows a byte"))),
                            Ok(v) => bytes.push((v & 0xff) as u8),
                            Err(e) => errors.push((n, e)),
                        }
                        pc = pc.wrapping_add(1);
                    }
                    emitted = true;
                }
                Stmt::Word(items) => {
                    for it in items {
                        match eval_expr(&it, pc, &labels, Ctx::Other, &mut unk) {
                            Ok(v) => {
                                bytes.push((v & 0xff) as u8);
                                bytes.push(((v >> 8) & 0xff) as u8);
                            }
                            Err(e) => errors.push((n, e)),
                        }
                        pc = pc.wrapping_add(2);
                    }
                    emitted = true;
                }
                Stmt::Insn(m, operand) => {
                    let mnemonic = m.to_ascii_lowercase();
                    let wide = wide_lines.contains(&idx);
                    let resolved = match resolve_operand(&mnemonic, &operand, pc, &labels, wide, &mut unk) {
                        Ok(r) => r,
                        Err(e) => {
                            errors.push((n, e));
                            continue;
                        }
                    };
                    if pass == 1 && !unk.is_empty() {
                        wide_lines.insert(idx);
                    }
                    let text = if resolved.is_empty() { mnemonic.clone() } else { format!("{mnemonic} {resolved}") };
                    match assemble_line(&text, pc) {
                        Ok(r) => {
                            pc = pc.wrapping_add(r.size);
                            bytes.extend(r.bytes);
                        }
                        // A forward branch's placeholder is its own address, always in
                        // range; any error in pass 2 is the source's.
                        Err(e) if pass == 2 => errors.push((n, e)),
                        Err(_) => pc = pc.wrapping_add(3),
                    }
                    emitted = true;
                }
            }
            if pass == 2 && !unk.is_empty() {
                errors.push((n, format!("undefined label: {}", unk.join(", "))));
            }
        }
        out.origin = org;
        if pass == 2 {
            out.errors = errors;
            out.bytes = if out.errors.is_empty() { bytes } else { vec![] };
        } else if !errors.is_empty() {
            out.errors = errors;
            return out;
        }
    }
    out.labels = labels.into_iter().collect();
    out
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

    fn block(src: &str, origin: u16) -> Vec<u8> {
        let b = assemble_block(src, origin);
        assert!(b.errors.is_empty(), "{:?}", b.errors);
        b.bytes
    }

    /// Spec 809 G6 — the assembler round-trips against `d`. Every documented opcode, as
    /// bytes, disassembled by the monitor's own `d`, the resulting SOURCE assembled back
    /// as one block: the same bytes. This is what "the monitor's `d` and `a` MUST agree"
    /// means once there is more than one line.
    #[test]
    fn g6_every_documented_instruction_round_trips_through_d() {
        use crate::addr_spans::{disasm_line, plain, Space};
        let origin: u16 = 0xc000;
        let mut image: Vec<u8> = vec![];
        let mut source = String::new();
        let mut count = 0;
        for op in 0u16..=0xff {
            let Some(e) = MICROCODE_TABLE[op as usize] else { continue };
            if !is_documented(e.op) {
                continue;
            }
            let at = origin.wrapping_add(image.len() as u16);
            // Operand bytes that disassemble unambiguously: a word above the zero page,
            // and a branch offset that stays in range.
            let probe = if e.mode == "rel" { [op as u8, 0x10, 0x00] } else { [op as u8, 0x34, 0x12] };
            let (size, text) = disasm_line(|a| probe[(a.wrapping_sub(at)) as usize % 3], at, Space::C64);
            image.extend_from_slice(&probe[..size as usize]);
            // `d` prints the address and the bytes before the mnemonic
            // (`$c00d  0d 34 12  ORA $1234`); the source is what follows them.
            let text = plain(&text);
            let src: Vec<&str> = text.split_whitespace().skip(size as usize + 1).collect();
            source.push_str(&src.join(" "));
            source.push('\n');
            count += 1;
        }
        assert!(count > 140, "the documented set is what is walked: {count}");
        assert_eq!(block(&source, origin), image, "source:\n{source}");
    }

    #[test]
    fn labels_resolve_backwards_and_forwards() {
        assert_eq!(block("loop: dex\n bne loop", 0xc000), vec![0xca, 0xd0, 0xfd]);
        assert_eq!(block(" beq done\n nop\ndone: rts", 0xc000), vec![0xf0, 0x01, 0xea, 0x60]);
        assert_eq!(block(" jsr sub\n rts\nsub: inc $d020\n rts", 0xc000),
            vec![0x20, 0x04, 0xc0, 0x60, 0xee, 0x20, 0xd0, 0x60]);
    }

    /// A label known before its use may fold to zero-page; one used before it is defined
    /// stays absolute in BOTH passes, or every address after it would move.
    #[test]
    fn a_forward_reference_is_sized_wide_and_stays_wide() {
        assert_eq!(block("zp = $fb\n lda zp", 0xc000), vec![0xa5, 0xfb]);
        assert_eq!(block(" lda zp\nzp = $fb", 0xc000), vec![0xad, 0xfb, 0x00]);
        let b = assemble_block(" lda zp\n nop\nhere: rts\nzp = $fb", 0xc000);
        assert!(b.errors.is_empty(), "{:?}", b.errors);
        assert_eq!(b.labels["here"], 0xc004, "the rts sits after a 3-byte lda");
    }

    #[test]
    fn low_high_star_arithmetic_and_data() {
        assert_eq!(block(" lda #<target\n ldx #>target\ntarget: rts", 0xc000), vec![0xa9, 0x04, 0xa2, 0xc0, 0x60]);
        assert_eq!(block(" sta ptr+1\nptr: .word $1234", 0xc000), vec![0x8d, 0x04, 0xc0, 0x34, 0x12]);
        assert_eq!(block(" jmp *", 0xc000), vec![0x4c, 0x00, 0xc0]);
        assert_eq!(block(".byte 1, $02, %11\n.word end\nend:", 0x1000), vec![1, 2, 3, 0x05, 0x10]);
        let b = assemble_block("*= $2000 ; the patch goes here\n rts", 0xc000);
        assert_eq!((b.origin, b.bytes.clone()), (0x2000, vec![0x60]), "{:?}", b.errors);
    }

    #[test]
    fn block_errors_carry_their_line() {
        let b = assemble_block(" nop\n lda nowhere", 0xc000);
        assert_eq!(b.errors[0].0, 2, "{:?}", b.errors);
        assert!(b.errors[0].1.contains("nowhere"), "{:?}", b.errors);
        assert!(b.bytes.is_empty(), "no bytes from a block that did not assemble");
        let far = format!(" beq far\n{}far: rts", " nop\n".repeat(200));
        assert!(!assemble_block(&far, 0xc000).errors.is_empty(), "a branch out of range is an error");
        let two = assemble_block(" nop\n*= $2000\n nop", 0xc000);
        assert!(two.errors.iter().any(|(n, e)| *n == 2 && e.contains("second segment")), "{:?}", two.errors);
        let dup = assemble_block("a1: nop\na1: nop", 0xc000);
        assert!(dup.errors.iter().any(|(_, e)| e.contains("twice")), "{:?}", dup.errors);
        let inc = assemble_block(".include \"x.s\"", 0xc000);
        assert!(inc.errors[0].1.contains("not a build system"), "{:?}", inc.errors);
    }

    #[test]
    fn errors() {
        assert!(assemble_line("lda #$1234", 0xc000).is_err()); // imm overflow
        assert!(assemble_line("foo $00", 0xc000).is_err()); // unknown mnemonic
        assert!(assemble_line("beq $d000", 0xc000).is_err()); // branch out of range
    }
}
