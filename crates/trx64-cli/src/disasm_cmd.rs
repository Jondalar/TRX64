//! `trx64cli disasm` — static, ROM-free disassembly of a PRG / raw image.
//!
//! Capability-cut migration step 1: no machine boot, no ROMs — file bytes go
//! straight through the shared `trx64-static` decoder (the SAME disassembler
//! the monitor `d` verb uses). PRG by default (2-byte load-address header);
//! `--load-address` switches to raw-image mode. Text output is the monitor
//! `d` line format; `--json` emits the `monitorDisasm` object shape.

use std::path::Path;

use serde_json::json;
use trx64_static::disasm6502::{disasm_line_ts, disasm_one, instr_len};

pub struct DisasmArgs<'a> {
    pub file: &'a Path,
    /// Raw-image mode: the whole file sits at this address (no PRG header).
    pub load_address: Option<u16>,
    /// First address to disassemble (defaults to the load address).
    pub start: Option<u16>,
    /// Maximum instructions to emit (defaults to "until end of image").
    pub count: Option<usize>,
    pub json: bool,
}

/// Parse a monitor-style address: `$c000`, `0xc000` or bare `c000` (hex).
pub fn parse_addr(s: &str) -> Result<u16, String> {
    let hex = s.strip_prefix('$').or_else(|| s.strip_prefix("0x")).unwrap_or(s);
    u16::from_str_radix(hex, 16).map_err(|_| format!("bad address '{s}' (hex: $c000 / 0xc000 / c000)"))
}

pub fn run_disasm(args: &DisasmArgs) -> Result<String, String> {
    let data = std::fs::read(args.file)
        .map_err(|e| format!("read {}: {e}", args.file.display()))?;

    let (load, payload): (u16, &[u8]) = match args.load_address {
        Some(a) => (a, &data[..]),
        None => {
            if data.len() < 2 {
                return Err(
                    "file too short for a PRG (2-byte load-address header); use --load-address for raw images"
                        .to_string(),
                );
            }
            ((data[0] as u16) | ((data[1] as u16) << 8), &data[2..])
        }
    };
    if payload.is_empty() {
        return Err("empty image (no bytes after the load address)".to_string());
    }

    // Image end, clamped at $ffff — a file running past the top of memory is
    // truncated (noted on stderr), not wrapped.
    let full_end = load as u32 + payload.len() as u32 - 1;
    let end = full_end.min(0xffff);
    if full_end > 0xffff {
        eprintln!(
            "warning: image runs past $ffff — truncated {} trailing byte(s)",
            full_end - 0xffff
        );
    }

    let start = match args.start {
        Some(s) => {
            if (s as u32) < load as u32 || s as u32 > end {
                return Err(format!(
                    "--start ${s:04x} outside image ${load:04x}-${:04x}",
                    end
                ));
            }
            s
        }
        None => load,
    };

    // Reads outside the image are zero-filled (a trailing partial instruction
    // decodes its missing operand bytes as $00), matching the ring-replay
    // convention in the daemon's chis renderer.
    let read = |a: u16| -> u8 {
        let a32 = a as u32;
        if a32 >= load as u32 && a32 <= end {
            payload[(a32 - load as u32) as usize]
        } else {
            0
        }
    };

    let max = args.count.unwrap_or(usize::MAX);
    let mut cursor = start as u32;
    let mut emitted = 0usize;
    let mut lines: Vec<String> = Vec::new();
    let mut objects: Vec<serde_json::Value> = Vec::new();

    while cursor <= end && emitted < max {
        let addr = cursor as u16;
        if args.json {
            let d = disasm_one(addr, read);
            objects.push(json!({
                "addr": d.addr,
                "bytes": d.bytes,
                "mnemonic": d.mnemonic,
                "operand": d.operand,
                "text": d.text
            }));
        } else {
            let (_, line) = disasm_line_ts(read, addr);
            lines.push(line);
        }
        let size = instr_len(read(addr)).max(1);
        cursor += size as u32;
        emitted += 1;
    }

    if args.json {
        serde_json::to_string_pretty(&objects).map_err(|e| e.to_string())
    } else {
        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        // Tests run in parallel — the name must be unique per test.
        p.push(format!("trx64_disasm_cmd_test_{}_{name}.prg", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn prg_header_and_text_listing() {
        // PRG: load $c000 — LDA #$0a / JSR $ffd2 / RTS
        let p = tmp("prg", &[0x00, 0xc0, 0xa9, 0x0a, 0x20, 0xd2, 0xff, 0x60]);
        let out = run_disasm(&DisasmArgs {
            file: &p,
            load_address: None,
            start: None,
            count: None,
            json: false,
        })
        .unwrap();
        assert_eq!(
            out,
            "$c000  a9 0a     LDA #$0a\n$c002  20 d2 ff  JSR $ffd2\n$c005  60        RTS"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn raw_mode_start_and_count() {
        // Raw at $1000: NOP NOP NOP
        let p = tmp("raw", &[0xea, 0xea, 0xea]);
        let out = run_disasm(&DisasmArgs {
            file: &p,
            load_address: Some(0x1000),
            start: Some(0x1001),
            count: Some(1),
            json: false,
        })
        .unwrap();
        assert_eq!(out, "$1001  ea        NOP");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn json_shape() {
        let p = tmp("json", &[0x00, 0xc0, 0xa9, 0x0a]);
        let out = run_disasm(&DisasmArgs {
            file: &p,
            load_address: None,
            start: None,
            count: None,
            json: true,
        })
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v[0]["addr"], 0xc000);
        assert_eq!(v[0]["text"], "$C000  A9 0A     LDA #$0A");
        std::fs::remove_file(&p).ok();
    }
}
