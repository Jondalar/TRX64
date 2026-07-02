#!/usr/bin/env npx tsx
// Regenerate the trx64-static disasm golden file from the TS oracle
// (C64RE `src/runtime/headless/debug/disasm6502.ts` — the `disasmLine` the
// monitor `d` verb renders). Run from the C64RE checkout so tsx + the TS
// source resolve:
//
//   cd ../C64ReverseEngineeringMCP && npx tsx ../TRX64/scripts/gen-disasm-goldens.mjs
//
// Output: crates/trx64-static/tests/goldens/disasm6502_goldens.json
// Cases: all 256 opcodes × two placements —
//   1. addr $c000, operand bytes 05 c0   (plain decode)
//   2. addr $fffe, operand bytes 80 ff   (address wraparound + negative branch)

import { writeFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const oracle = join(
  here,
  "..",
  "..",
  "C64ReverseEngineeringMCP",
  "src",
  "runtime",
  "headless",
  "debug",
  "disasm6502.ts",
);
const { disasmLine } = await import(oracle);

const cases = [];
for (const [addr, b1, b2] of [
  [0xc000, 0x05, 0xc0],
  [0xfffe, 0x80, 0xff],
]) {
  for (let op = 0; op <= 0xff; op++) {
    const read = (a) => {
      const off = (a - addr) & 0xffff;
      return off === 0 ? op : off === 1 ? b1 : off === 2 ? b2 : 0;
    };
    const { size, line } = disasmLine(read, addr);
    cases.push({ op, addr, b1, b2, size, line });
  }
}

const out = join(here, "..", "crates", "trx64-static", "tests", "goldens");
mkdirSync(out, { recursive: true });
const file = join(out, "disasm6502_goldens.json");
writeFileSync(file, JSON.stringify(cases, null, 1) + "\n");
console.log(`${cases.length} cases -> ${file}`);
