//! trx64-static — the machine-free 6502 decoder the runtime uses.
//!
//! Capability-cut migration step 1 (`docs/capability-cut-decisions.md`): the raw
//! 6502 decode/format layer, extracted from trx64-daemon so the daemon (monitor
//! `d` / `chis` / flow walks) and the CLI (`trx64cli disasm`, ROM-free) share ONE
//! disassembler. Depends only on `trx64-core` tables (MICROCODE_TABLE /
//! UNDOC_TABLE — full 256 opcodes incl. undocumented).
//!
//! Boundary: neutral decode, no meaning. TRX64 is a runtime (revised 2026-09-19):
//! static analysis — classifiers, media parsing, semantic disassembly, KickAsm
//! emission, byte-verify rebuild — is C64RE's and does not move here. The golden
//! suite holds this decoder to C64RE's TS decoder.

pub mod disasm6502;
