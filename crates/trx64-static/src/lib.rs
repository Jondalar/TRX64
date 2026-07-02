//! trx64-static — static (machine-free) capability: decode / parse / classify.
//!
//! Capability-cut migration step 1 (`docs/capability-cut-decisions.md`,
//! "Migration order"): the raw 6502 decode/format layer, extracted from
//! trx64-daemon so the daemon (monitor `d` / `chis` / flow walks), the CLI
//! (`trx64cli disasm`, ROM-free) and the future `trx64-mcp` façade share ONE
//! disassembler. Depends only on `trx64-core` tables (MICROCODE_TABLE /
//! UNDOC_TABLE — full 256 opcodes incl. undocumented).
//!
//! Boundary (capability-cut Q1-C): this crate is CAPABILITY — neutral decode,
//! no meaning. Semantic disassembly (annotations, SegmentKind mapping, KickAsm
//! emission, byte-verify rebuild) stays in C64RE. Media format-parse (step 2)
//! and the heuristic classifiers (step 3) land here when they migrate.

pub mod disasm6502;
