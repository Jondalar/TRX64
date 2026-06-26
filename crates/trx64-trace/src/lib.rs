//! trx64-trace — binary TraceOp encoder.
//!
//! Writes `.c64retrace` frames byte-identical to the TS `binary-format.ts` /
//! `binary-log-writer.ts` (the immovable trace format = the conformance oracle).
//! Implements [`trx64_core::Observer`], so the core stays agnostic to the sink.
//!
//! On-disk layout (little-endian), authoritative per binary-format.ts:
//!   FileHeader := MAGIC(8) version(u16) flags(u16) metaLen(u32) metaJson(metaLen)
//!   CPU_STEP (0x10):        op(1) cycle(f64) pc(u16) opcode(u8) a x y sp p b1 b2  = 19 bytes
//!   RAM/IO_WRITE (0x11/0x12): op(1) cycle(f64) addr(u16) value(u8) pc(u16)
//!       access(u8, bit0=r/w, bit7=hasOld) oldValue(u8)                            = 16 bytes
//!   DRIVE_CPU_STEP (0x30):  same layout as CPU_STEP — 19 bytes total
//!   DRIVE_RAM_WRITE (0x31): same layout as RAM_WRITE — 16 bytes total
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
    /// Drive 1541 CPU instruction retire (op 0x30 — binary-format.ts DRIVE_CPU_STEP).
    DriveCpuStep = 0x30,
    /// Drive 1541 memory bus access (op 0x31 — binary-format.ts DRIVE_RAM_WRITE).
    DriveRamWrite = 0x31,
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

    /// Encode a DRIVE_CPU_STEP (0x30) record — same layout as CPU_STEP (0x10).
    ///
    /// Emitted by the `drive8-cpu` trace domain (sampled at C64 instruction
    /// boundaries, deduplicated by PC). The `opcode`/`b1`/`b2` fields are 0 in
    /// the sampled path (the TS oracle does not observe per-instruction operands
    /// from the drive; see integrated-session.ts:855 ADR-015 note).
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn write_drive_cpu_step(
        &mut self,
        cycle: u64,
        pc: u16,
        a: u8,
        x: u8,
        y: u8,
        sp: u8,
        p: u8,
    ) {
        self.buf.push(TraceOp::DriveCpuStep as u8);
        self.buf.extend_from_slice(&(cycle as f64).to_le_bytes());
        self.buf.extend_from_slice(&pc.to_le_bytes());
        self.buf.push(0); // opcode: not observable in sampled mode
        self.buf.push(a);
        self.buf.push(x);
        self.buf.push(y);
        self.buf.push(sp);
        self.buf.push(p);
        self.buf.push(0); // b1: not observable
        self.buf.push(0); // b2: not observable
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

    /// reverse-debug Phase 1c — encode ONE always-on delta-ring entry (its CPU
    /// PRE-state header + every write it performed) as the trace's CPU_STEP (0x10) +
    /// RAM_WRITE (0x11) records, so a `trace/build_from_ring` dump round-trips through
    /// the EXISTING sidecar read path (swimlane / map / taint) identically to a live
    /// trace. Returns the number of records written (1 cpu row + N mem rows).
    ///
    /// FIELD MAPPING (ring → trace), and the two documented GAPS:
    ///  * `cycle` ← `e.cycle` (the instruction's clock stamp) — EXACT; this is the
    ///    field swimlane/map/taint query on, so the window is faithful.
    ///  * `pc`    ← `e.pc` (the opcode address) — EXACT.
    ///  * `a/x/y/sp/p` ← the ring's PRE-execute registers. GAP vs a live CPU_STEP,
    ///    which carries POST-execute regs (and `p` with N/Z masked): the always-on
    ///    delta ring stores the PRE-state (the state reverse-step lands on), so a
    ///    rebuilt row reports pre-instruction regs. The PC/cycle the readers key on are
    ///    unaffected; the reg columns are pre- not post-state. Self-consistent.
    ///  * `opcode/b1/b2` ← `e.opcode`/`e.b1`/`e.b2`. REAL: the delta entry now carries
    ///    the decoded opcode + operand bytes, stamped at retire by `DeltaRing::set_opcode`
    ///    from the SAME fields the `cpu_history` ring receives (no re-decode). So a
    ///    rebuilt trace's disasm column shows real mnemonics (LDA/STA/JMP/…), not a blank
    ///    / BRK-for-every-row column. (An interrupt-only dispatch with no opcode body
    ///    leaves these at 0 — the same as a live trace's interrupt rows.)
    ///  * each write → RAM_WRITE (0x11) with `value`=new, `pc`=`e.pc`, `cycle`=`e.cycle`,
    ///    and `old_value` carried under the SAME rule as the live `on_bus` tap (Spec
    ///    753): only for a write to the side-effect-free RAM window `$0002..$D000`.
    pub fn write_delta_entry(
        &mut self,
        e: &trx64_core::DeltaEntry,
        writes: &[trx64_core::WriteRec],
    ) -> u64 {
        // CPU row (PRE-state regs; opcode/operands are REAL, stamped at retire).
        self.write_cpu_step(e.cycle, e.pc, e.opcode, e.a, e.x, e.y, e.sp, e.p, e.b1, e.b2);
        let mut count = 1u64;
        for w in writes {
            // Same old-value rule as TracingObserver::on_bus (Spec 753): carry the
            // pre-write byte only inside the RAM window; I/O-window writes omit it.
            let old_opt = if (0x0002..0xd000).contains(&w.addr) { Some(w.old_value) } else { None };
            self.write_mem_access(TraceOp::RamWrite, e.cycle, w.addr, w.new_value, e.pc, ACCESS_WRITE, old_opt);
            count += 1;
        }
        count
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
/// (+io), vic→vic, iec→iec, sid→sid, drive8-cpu→drive_cpu. A record is emitted
/// ONLY if its channel is enabled.
///
/// NOTE (ADR-015-style empirical finding): the `sid` channel is RESERVED with
/// NO live producer in the TS oracle — confirmed by tracing a SID exerciser:
/// SID register writes appear only as op-0x11 RAM_WRITE from the CPU bus tap,
/// never as op-0x22 SID_REG_WRITE. The `sid` field here only selects the SID
/// isolation bus for the run; no SID trace frames are ever emitted.
#[derive(Clone, Copy, Debug)]
pub struct TraceChannels {
    /// `cpu` channel — emits CPU_STEP (0x10).
    pub cpu: bool,
    /// `bus_access` + `io` channels — emit RAM_WRITE (0x11) / IO_WRITE (0x12).
    pub mem: bool,
    /// `vic` channel — emits VIC_REG_WRITE (0x20). NO live producer (reserved).
    pub vic: bool,
    /// `sid` channel — RESERVED, NO live producer. Activates the SID isolation
    /// bus so the exerciser runs against the SID model, but op-0x22 is never
    /// emitted. The `sid` domain enables ONLY this channel (= TS: sid → {"sid"});
    /// it does NOT co-enable `cpu`/`memory` (audit formats-state-6).
    pub sid: bool,
    /// `drive_pc` channel — emits DRIVE_CPU_STEP (0x30). Activated by "drive8-cpu"
    /// domain. Sampled at C64 instruction boundaries, deduplicated by PC.
    pub drive_cpu: bool,
}

impl TraceChannels {
    /// Map trace domains → channels (= TS domainsToChannels).
    pub fn from_domains<S: AsRef<str>>(domains: &[S]) -> Self {
        let mut c = TraceChannels { cpu: false, mem: false, vic: false, sid: false, drive_cpu: false };
        for d in domains {
            match d.as_ref() {
                "c64-cpu" => c.cpu = true,
                "memory" => c.mem = true,
                "vic" | "c64-vic" => c.vic = true,
                // audit formats-state-6 — `sid` enables ONLY the sid channel (= TS
                // domainsToChannels: sid → {"sid"}). The sid channel has no live
                // producer, so a sid-only domain yields an empty stream; it must NOT
                // co-enable cpu/mem (that wrongly inflated a sid trace with every
                // CPU step + RAM/IO write).
                "sid" => { c.sid = true; }
                "drive8-cpu" => c.drive_cpu = true,
                _ => {}
            }
        }
        c
    }

    /// Default capture set (no domains given) = cpu + memory (TS daemon default).
    pub fn default_cpu_mem() -> Self {
        TraceChannels { cpu: true, mem: true, vic: true, sid: false, drive_cpu: false }
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

    /// Emit a DRIVE_CPU_STEP record directly (called from the daemon's drive-sampled
    /// run loop, not via the Observer trait — the drive CPU runs with NullSink).
    #[inline]
    pub fn emit_drive_step(&mut self, pc: u16, a: u8, x: u8, y: u8, sp: u8, p: u8, drv_clk: u64) {
        if !self.channels.drive_cpu {
            return;
        }
        self.sink.write_drive_cpu_step(drv_clk, pc, a, x, y, sp, p);
        self.event_count += 1;
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
        // EMPIRICAL (ADR-015-style, CIA gate): the TS oracle's binary trace is
        // produced by the `bus_access` CPU per-access tap (Spec 142), which emits
        // op 0x11 (RAM_WRITE) for EVERY C64 access regardless of region — including
        // the $D000-$DFFF I/O window (VIC/SID/CIA1/CIA2). The op-0x12 (IO_WRITE)
        // code is a RESERVED opcode with NO live producer in this build: a CIA
        // exerciser ($DC00-$DDFF reads+writes) and even a full BASIC boot emit
        // ZERO io frames — $D016/$DC0D/$DD0D all come through as op 0x11. So we
        // emit RAM_WRITE for all bus accesses; routing $Dxxx → IoWrite diverged at
        // the first $DC04 write (trace[2].family expected="ram" got="io").
        let op = TraceOp::RamWrite;
        // oldValue: the TS writer carries the pre-write byte ONLY for writes in the
        // side-effect-free RAM window ($0002..$D000). Reads and I/O-window writes
        // ($D000-$DFFF) omit it (hasOld=0 ⇒ access byte 0x01, old=0) — Spec 753.
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
