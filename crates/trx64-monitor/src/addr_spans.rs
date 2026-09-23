//! addr_spans.rs — WHERE the monitor printed an address (Spec 804).
//!
//! TRX64 holds no symbols; C64RE names addresses. It must never find an address by
//! parsing a formatted text column, so the runtime says where its addresses are: every
//! `monitor/exec` reply carries `spans` — per line, the column range, the address, its
//! space (`c64` | `drive8` … `drive11`) and its role (`pc` | `target` | `operand` | `memory`).
//!
//! The runtime knows those positions because it formatted them. A formatter wraps an
//! address it prints in an in-band MARK (private-use code points, never printable
//! monitor output); the one exit of `monitor/exec` strips the marks and computes the
//! positions from the text as it finally is — after every `format!`, prefix, suffix and
//! `join` the verb applied. Every other caller of the monitor gets the plain text,
//! byte-identical to what it was before the marks existed.
//!
//! Coverage is a property of the formatter, not of the verb: a verb whose output passes
//! through a marked formatter has spans; one that does not answers `spans: []`. Nothing
//! here scans text for things that look like addresses — no mark, no span.

use serde_json::{json, Value};
use trx64_static::disasm6502::{disasm_line_ts_spans, SpanRole};

const START: char = '\u{E000}';
const SEP: char = '\u{E001}';
const END: char = '\u{E002}';

/// Which CPU an address belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    C64,
    /// A 1541, by the unit it answers to (Spec 871).
    Drive(u8),
}

impl Space {
    fn as_str(self) -> &'static str {
        match self {
            Space::C64 => "c64",
            Space::Drive(u) => crate::host::Device::Drive(u).name(),
        }
    }
}

/// What a printed address IS (the contract's `role`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// An instruction's own address, the CPU's PC, a writer PC, a backtrace frame.
    Pc,
    /// A branch/JSR/JMP destination, a vector's contents.
    Target,
    /// Any other address an instruction references.
    Operand,
    /// An address shown as data: a dump row, a written address, a stack slot.
    Memory,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Pc => "pc",
            Role::Target => "target",
            Role::Operand => "operand",
            Role::Memory => "memory",
        }
    }
    fn from_str(s: &str) -> Option<Role> {
        Some(match s {
            "pc" => Role::Pc,
            "target" => Role::Target,
            "operand" => Role::Operand,
            "memory" => Role::Memory,
            _ => return None,
        })
    }
}

/// One address the monitor printed, located in the final reply text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddrSpan {
    /// Index into the reply split on `\n`.
    pub line: usize,
    /// UTF-16 code-unit offsets in that line (what a JS `slice` takes), end exclusive.
    pub start: usize,
    pub end: usize,
    pub addr: u16,
    pub space: Space,
    pub role: Role,
    /// The bank lens the bytes were read through, when it is not the CPU's view.
    pub lens: Option<String>,
    /// A range span (a dump row) covers `len` bytes from `addr`; a point span is 1.
    pub len: u16,
}

impl AddrSpan {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "line": self.line,
            "start": self.start,
            "end": self.end,
            "addr": self.addr,
            "space": self.space.as_str(),
            "role": self.role.as_str(),
        });
        if let Some(l) = &self.lens {
            v["lens"] = json!(l);
        }
        if self.len > 1 {
            v["len"] = json!(self.len);
        }
        v
    }
}

/// Mark `visible` (the text as printed) as the address `addr`.
pub fn mark(visible: &str, addr: u16, space: Space, role: Role, lens: Option<&str>, len: u16) -> String {
    let lens = match lens {
        Some(l) if l != "cpu" && !l.is_empty() => l,
        _ => "",
    };
    format!(
        "{START}{},{},{},{:04x},{:04x}{SEP}{visible}{END}",
        role.as_str(),
        space.as_str(),
        lens,
        addr,
        len.max(1)
    )
}

/// `$xxxx`, marked — the house format for a 16-bit address.
pub fn addr4(addr: u16, space: Space, role: Role) -> String {
    mark(&format!("${addr:04x}"), addr, space, role, None, 1)
}

/// A disassembly line with its own address and its operand address marked. The text,
/// once stripped, is exactly `disasm_line_ts`.
pub fn disasm_line(read: impl Fn(u16) -> u8, addr: u16, space: Space) -> (u16, String) {
    disasm_line_in(read, addr, space, None)
}

/// [`disasm_line`] for bytes read through a bank lens (`d ram a000`): the spans say so.
pub fn disasm_line_in(read: impl Fn(u16) -> u8, addr: u16, space: Space, lens: Option<&str>) -> (u16, String) {
    let (size, line, spans) = disasm_line_ts_spans(read, addr);
    let mut out = line.clone();
    // Right to left, so earlier offsets stay valid. The line is ASCII: byte offsets are
    // character offsets.
    for sp in spans.iter().rev() {
        let role = match sp.role {
            SpanRole::Pc => Role::Pc,
            SpanRole::Target => Role::Target,
            SpanRole::Operand => Role::Operand,
        };
        let marked = mark(&line[sp.start..sp.end], sp.addr, space, role, lens, 1);
        out.replace_range(sp.start..sp.end, &marked);
    }
    (size, out)
}

/// The number of characters a reader sees — marks excluded. For the few formatters that
/// pad a marked string to a column (`format!("{:<30}")` would count the marks).
pub fn visible_len(s: &str) -> usize {
    let mut n = 0;
    let mut in_meta = false;
    for c in s.chars() {
        match c {
            START => in_meta = true,
            SEP => in_meta = false,
            END => {}
            _ if in_meta => {}
            _ => n += 1,
        }
    }
    n
}

/// Left-align `s` in `width` visible columns (the marked form of `format!("{s:<width$}")`).
pub fn pad_right(s: &str, width: usize) -> String {
    let n = visible_len(s);
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

/// The text a reader sees: every mark removed. Cheap when there is none.
pub fn plain(text: &str) -> String {
    if !text.contains(START) {
        return text.to_string();
    }
    strip(text).0
}

/// Remove the marks and say where each marked address ended up.
pub fn strip(text: &str) -> (String, Vec<AddrSpan>) {
    if !text.contains(START) {
        return (text.to_string(), Vec::new());
    }
    let mut out = String::with_capacity(text.len());
    let mut spans = Vec::new();
    for (line_no, line) in text.split('\n').enumerate() {
        if line_no > 0 {
            out.push('\n');
        }
        let mut col: usize = 0; // UTF-16 units in the stripped line
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c != START {
                if c != SEP && c != END {
                    out.push(c);
                    col += c.len_utf16();
                }
                continue;
            }
            let mut meta = String::new();
            for m in chars.by_ref() {
                if m == SEP {
                    break;
                }
                meta.push(m);
            }
            let start = col;
            for v in chars.by_ref() {
                if v == END {
                    break;
                }
                out.push(v);
                col += v.len_utf16();
            }
            if let Some(span) = parse_meta(&meta, line_no, start, col) {
                spans.push(span);
            }
        }
    }
    (out, spans)
}

fn parse_meta(meta: &str, line: usize, start: usize, end: usize) -> Option<AddrSpan> {
    let mut it = meta.split(',');
    let role = Role::from_str(it.next()?)?;
    let space = match it.next()? {
        "c64" => Space::C64,
        other => Space::Drive(crate::host::Device::drive_unit(other)?),
    };
    let lens = it.next().filter(|l| !l.is_empty()).map(String::from);
    let addr = u16::from_str_radix(it.next()?, 16).ok()?;
    let len = u16::from_str_radix(it.next()?, 16).ok()?;
    if end <= start {
        return None;
    }
    Some(AddrSpan { line, start, end, addr, space, role, lens, len })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice16(line: &str, start: usize, end: usize) -> String {
        let u: Vec<u16> = line.encode_utf16().collect();
        String::from_utf16(&u[start..end]).unwrap()
    }

    #[test]
    fn a_marked_address_survives_formatting_and_lands_where_it_is_printed() {
        let a = addr4(0xc000, Space::C64, Role::Pc);
        let b = addr4(0x0400, Space::C64, Role::Memory);
        let text = format!("head line\n  → {a}: wrote {b}   tail");
        let (plain_text, spans) = strip(&text);
        assert_eq!(plain_text, "head line\n  → $c000: wrote $0400   tail");
        assert_eq!(spans.len(), 2);
        let line = plain_text.split('\n').nth(1).unwrap();
        assert_eq!(slice16(line, spans[0].start, spans[0].end), "$c000");
        assert_eq!((spans[0].line, spans[0].addr, spans[0].role), (1, 0xc000, Role::Pc));
        assert_eq!(slice16(line, spans[1].start, spans[1].end), "$0400");
        assert_eq!(spans[1].role, Role::Memory);
    }

    #[test]
    fn the_plain_text_is_the_unmarked_text() {
        let (_, marked) = disasm_line(|a| [0x20u8, 0xd2, 0xff][(a.wrapping_sub(0xc000)) as usize % 3], 0xc000, Space::C64);
        assert_eq!(plain(&marked), "$c000  20 d2 ff  JSR $ffd2");
        assert_eq!(plain("no marks here"), "no marks here");
    }

    #[test]
    fn padding_counts_what_a_reader_sees() {
        let (_, marked) = disasm_line(|_| 0xea, 0x1000, Space::C64);
        let padded = pad_right(&marked, 30);
        assert_eq!(plain(&padded), format!("{:<30}", "$1000  ea        NOP"));
    }

    #[test]
    fn a_range_span_carries_its_length_and_lens() {
        let m = mark("c000", 0xc000, Space::Drive(8), Role::Memory, Some("ram"), 32);
        let (_, spans) = strip(&format!(">R:{m}  00 01"));
        assert_eq!(spans[0].to_json(), json!({
            "line": 0, "start": 3, "end": 7, "addr": 0xc000, "space": "drive8",
            "role": "memory", "lens": "ram", "len": 32
        }));
    }
}
