//! Golden parity vs the TS oracle — `disasm_line_ts` must render byte-identical
//! to C64RE `disasm6502.ts` `disasmLine` ("old TS path retired only after
//! parity", capability-cut-decisions.md Migration order §4).
//!
//! Regenerate the goldens (all 256 opcodes × 2 placements incl. $fffe wrap):
//!   cd ../C64ReverseEngineeringMCP && npx tsx ../TRX64/scripts/gen-disasm-goldens.mjs

use trx64_static::disasm6502::disasm_line_ts;

#[test]
fn disasm_line_matches_ts_oracle() {
    let raw = include_str!("goldens/disasm6502_goldens.json");
    let cases: serde_json::Value = serde_json::from_str(raw).expect("goldens JSON");
    let cases = cases.as_array().expect("goldens array");
    assert_eq!(cases.len(), 512, "expected 256 opcodes x 2 placements");

    for c in cases {
        let op = c["op"].as_u64().unwrap() as u8;
        let addr = c["addr"].as_u64().unwrap() as u16;
        let b1 = c["b1"].as_u64().unwrap() as u8;
        let b2 = c["b2"].as_u64().unwrap() as u8;
        let read = move |a: u16| -> u8 {
            match a.wrapping_sub(addr) {
                0 => op,
                1 => b1,
                2 => b2,
                _ => 0,
            }
        };
        let (size, line) = disasm_line_ts(read, addr);
        assert_eq!(
            size as u64,
            c["size"].as_u64().unwrap(),
            "size mismatch op=${op:02x} addr=${addr:04x}"
        );
        assert_eq!(
            line,
            c["line"].as_str().unwrap(),
            "line mismatch op=${op:02x} addr=${addr:04x}"
        );
    }
}
