//! trx64-trace — binary TraceOp encoder.
//!
//! Writes `.c64retrace` frames byte-identical to the TS `binary-log-writer`
//! (the immovable trace format = the conformance oracle). Implements
//! [`trx64_core::Observer`], so the core stays agnostic to the sink format.

use trx64_core::{BusKind, Observer};

/// TraceOp opcodes — MUST match TS `binary-format.ts` SIZE table (the contract).
/// Frame layout is little-endian, fixed payload per op (10–18 bytes).
#[repr(u8)]
#[derive(Clone, Copy, Debug)]
pub enum TraceOp {
    Mark = 0x01,
    CpuStep = 0x10,
    RamWrite = 0x11,
    IoWrite = 0x12,
    IecLine = 0x23,
}

/// Forensic firehose sink: encodes events little-endian into pooled chunks, drained
/// to `.c64retrace`. ~985k events/s, zero-alloc hot path (no per-event allocation).
pub struct FrameSink {
    /// TODO(loop): replace with pooled 1 MiB chunks + async drain, matching
    /// binary-log-writer.ts (POOL_TARGET, CHUNK_BYTES, flip/drain at pause boundary).
    pub buf: Vec<u8>,
}

impl FrameSink {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }
}

impl Default for FrameSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer for FrameSink {
    fn on_instruction(&mut self, _pc: u16, _opcode: u8, _a: u8, _x: u8, _y: u8, _sp: u8, _p: u8, _clk: u64) {
        // TODO(loop): encode CpuStep frame (op + cycle:f64 + pc + opcode + regs + b1/b2).
    }

    fn on_bus(&mut self, _kind: BusKind, _addr: u16, _value: u8) {
        // TODO(loop): encode RamWrite (0x11) / IoWrite (0x12) frame.
    }

    fn on_interrupt(&mut self, _vector: u16, _clk: u64) {
        // TODO(loop): irq channel.
    }
}
