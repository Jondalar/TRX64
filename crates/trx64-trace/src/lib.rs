//! trx64-trace — binary TraceOp encoder.
//!
//! Writes `.c64retrace` frames byte-identical to the TS `binary-format.ts` /
//! `binary-log-writer.ts` (the immovable trace format = the conformance oracle).
//! Implements [`trx64_core::Observer`], so the core stays agnostic to the sink.
//!
//! On-disk layout (little-endian), authoritative per binary-format.ts:
//!   FileHeader := MAGIC(8) version(u16) flags(u16) metaLen(u32) metaJson(metaLen)
//!   CPU_STEP (0x10): op(1) cycle(f64) pc(u16) opcode(u8) a x y sp p b1 b2   = 19 bytes
//!   RAM/IO_WRITE (0x11/0x12): op(1) cycle(f64) addr(u16) value(u8) pc(u16)
//!       access(u8, bit0=r/w, bit7=hasOld) oldValue(u8)                      = 16 bytes
//!
//! access/oldValue (Spec 753): WRITE records carry the pre-write value for RAM
//! (addr in $0002..$D000); reads and I/O-window writes omit it (hasOld=0).

use trx64_core::{BusKind, Observer};

/// TraceOp opcodes — MUST match TS `binary-format.ts` (the contract).
#[repr(u8)]
#[derive(Clone, Copy, Debug)]
pub enum TraceOp {
    Mark = 0x01,
    CpuStep = 0x10,
    RamWrite = 0x11,
    IoWrite = 0x12,
    VicRegWrite = 0x20,
    IecLine = 0x23,
}

pub const MAGIC: &[u8; 8] = b"C64RETR1";
pub const FORMAT_VERSION: u16 = 2;

pub const ACCESS_READ: u8 = 0;
pub const ACCESS_WRITE: u8 = 1;

/// Forensic firehose sink: encodes events little-endian into a growable buffer.
pub struct FrameSink {
    pub buf: Vec<u8>,
}

impl FrameSink {
    /// Create a sink with the file header already written, capturing `meta` JSON.
    pub fn with_header(meta_json: &str) -> Self {
        let mut buf = Vec::with_capacity(1 << 16);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // flags
        let meta = meta_json.as_bytes();
        buf.extend_from_slice(&(meta.len() as u32).to_le_bytes());
        buf.extend_from_slice(meta);
        Self { buf }
    }

    /// Create a header-less sink (events appended to an existing file's stream).
    pub fn events_only() -> Self {
        Self { buf: Vec::with_capacity(1 << 16) }
    }

    #[inline]
    fn write_cpu_step(
        &mut self,
        cycle: u64,
        pc: u16,
        opcode: u8,
        a: u8,
        x: u8,
        y: u8,
        sp: u8,
        p: u8,
        b1: u8,
        b2: u8,
    ) {
        self.buf.push(TraceOp::CpuStep as u8);
        self.buf.extend_from_slice(&(cycle as f64).to_le_bytes());
        self.buf.extend_from_slice(&pc.to_le_bytes());
        self.buf.push(opcode);
        self.buf.push(a);
        self.buf.push(x);
        self.buf.push(y);
        self.buf.push(sp);
        self.buf.push(p);
        self.buf.push(b1);
        self.buf.push(b2);
    }

    /// Encode a VIC_REG_WRITE frame (op 0x20, 13 bytes total: op + cycle f64 +
    /// rasterY u16 + kind u8 + value u8) — byte-identical to TS
    /// binary-format.ts `encodeVicEvent`. kind = VIC_KIND_CODE
    /// (1=raster,2=mode,3=irq,4=badline).
    ///
    /// RESERVED in practice: the TS oracle's vic channel has no live producer, so
    /// a parity trace never contains these. Provided for binary-format
    /// completeness + future machine-integration use.
    #[inline]
    pub fn write_vic_event(&mut self, cycle: u64, raster_y: u16, kind: u8, value: u8) {
        self.buf.push(TraceOp::VicRegWrite as u8);
        self.buf.extend_from_slice(&(cycle as f64).to_le_bytes());
        self.buf.extend_from_slice(&raster_y.to_le_bytes());
        self.buf.push(kind);
        self.buf.push(value);
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn write_mem_access(
        &mut self,
        op: TraceOp,
        cycle: u64,
        addr: u16,
        value: u8,
        pc: u16,
        access: u8,
        old: Option<u8>,
    ) {
        self.buf.push(op as u8);
        self.buf.extend_from_slice(&(cycle as f64).to_le_bytes());
        self.buf.extend_from_slice(&addr.to_le_bytes());
        self.buf.push(value);
        self.buf.extend_from_slice(&pc.to_le_bytes());
        let has_old = old.is_some();
        self.buf.push((access & 0x7f) | if has_old { 0x80 } else { 0 });
        self.buf.push(old.unwrap_or(0));
    }
}

/// Active trace channels, derived from the requested trace domains exactly like
/// TS `domainsToChannels` (trace-definition.ts): c64-cpu→cpu, memory→bus_access
/// (+io), vic→vic, iec→iec, sid→sid. A record is emitted ONLY if its channel is
/// enabled. This is the load-bearing parity rule for chip-isolated traces: a
/// vic-domain trace enables ONLY the `vic` channel, which has no producer, so
/// the trace is empty — byte-identical to the TS oracle.
#[derive(Clone, Copy, Debug)]
pub struct TraceChannels {
    /// `cpu` channel — emits CPU_STEP (0x10).
    pub cpu: bool,
    /// `bus_access` + `io` channels — emit RAM_WRITE (0x11) / IO_WRITE (0x12).
    pub mem: bool,
    /// `vic` channel — emits VIC_REG_WRITE (0x20). NO live producer (reserved).
    pub vic: bool,
}

impl TraceChannels {
    /// Map trace domains → channels (= TS domainsToChannels).
    pub fn from_domains<S: AsRef<str>>(domains: &[S]) -> Self {
        let mut c = TraceChannels { cpu: false, mem: false, vic: false };
        for d in domains {
            match d.as_ref() {
                "c64-cpu" => c.cpu = true,
                "memory" => c.mem = true,
                "vic" | "c64-vic" => c.vic = true,
                _ => {}
            }
        }
        c
    }

    /// Default capture set (no domains given) = cpu + memory (TS daemon default).
    pub fn default_cpu_mem() -> Self {
        TraceChannels { cpu: true, mem: true, vic: true }
    }
}

/// Streaming trace observer: encodes CpuStep + RAM/IO mem-access frames as the
/// CPU executes, FILTERED by the active [`TraceChannels`]. Bus events carry the
/// live reg_pc + clk + pre-write old byte so each record is stamped exactly as
/// the TS writer does.
pub struct TracingObserver {
    pub sink: FrameSink,
    pub event_count: u64,
    pub channels: TraceChannels,
}

impl TracingObserver {
    /// New observer capturing cpu + memory (the daemon default).
    pub fn new(sink: FrameSink) -> Self {
        Self { sink, event_count: 0, channels: TraceChannels::default_cpu_mem() }
    }
    /// New observer capturing only the given channels.
    pub fn with_channels(sink: FrameSink, channels: TraceChannels) -> Self {
        Self { sink, event_count: 0, channels }
    }
    pub fn into_buf(self) -> Vec<u8> {
        self.sink.buf
    }
}

impl Observer for TracingObserver {
    fn on_instruction(
        &mut self,
        pc: u16,
        opcode: u8,
        b1: u8,
        b2: u8,
        a: u8,
        x: u8,
        y: u8,
        sp: u8,
        p: u8,
        clk: u64,
    ) {
        if !self.channels.cpu {
            return;
        }
        self.sink.write_cpu_step(clk, pc, opcode, a, x, y, sp, p, b1, b2);
        self.event_count += 1;
    }

    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, pc: u16, clk: u64, old: u8) {
        if !self.channels.mem {
            return;
        }
        // integrated-session.ts forwards only WRITE + READ to the producer;
        // FETCH and DUMMY_* are NOT emitted to the trace. The op byte
        // distinguishes read vs write.
        let access = match kind {
            BusKind::Write => ACCESS_WRITE,
            BusKind::Read => ACCESS_READ,
            _ => return,
        };
        // I/O window = $D000..$DFFF -> IO_WRITE (0x12); else RAM_WRITE (0x11).
        let op = if (0xd000..0xe000).contains(&addr) {
            TraceOp::IoWrite
        } else {
            TraceOp::RamWrite
        };
        // oldValue: only for RAM writes in the side-effect-free window
        // ($0002..$D000). Reads + I/O writes omit it (Spec 753).
        let old_opt = if access == ACCESS_WRITE && (0x0002..0xd000).contains(&addr) {
            Some(old)
        } else {
            None
        };
        self.sink.write_mem_access(op, clk, addr, value, pc, access, old_opt);
        self.event_count += 1;
    }

    fn on_interrupt(&mut self, _vector: u16, _clk: u64) {}
}
