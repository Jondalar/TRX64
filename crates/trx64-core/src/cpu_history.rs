//! cpu_history.rs — reverse-debug Phase 1a: the always-on in-memory CPU-history ring.
//!
//! A fixed-capacity circular buffer of the last N retired C64 CPU instructions, fed
//! from the EXISTING per-instruction execution point (`full_sc::execute_one`, right
//! beside the `Observer::on_instruction` trace hook — no second decode, no second
//! step). This is the VICE `cpuhistory` model: a live ring the monitor `chis` verb
//! reads with no trace / finalize / sidecar dependency.
//!
//! WHY a dedicated ring (not the trace firehose): the firehose (`.c64retrace`) is
//! only written while a trace is ACTIVE and is read only after `trace off` + index
//! build (the historical path). The user runs WITH the trace on and types `chis` →
//! they need the LAST N instructions LIVE, from RAM. This ring is that source.
//!
//! HOT-PATH DISCIPLINE (this runs at ~1 MHz): `push` is a handful of field stores
//! into a pre-allocated slab + one index advance + overwrite-oldest. NO allocation,
//! NO formatting, NO per-push branch beyond the `enabled` gate. The slab is built
//! once at `Machine::new`; `push` never grows it. The entry is 24 bytes
//! (`#[repr(C)]`: u64 cycle + u16 pc + 8×u8 regs/operands, padded to the u64
//! alignment), so a 256k-entry ring is 6 MiB — bounded.
//!
//! KILL-SWITCH: default-ON. `TRX64_CPUHISTORY=0` (or `off`/`false`) disables the
//! push (the ring stays empty, `chis` falls back to the finalized-trace path).
//!
//! CLONE / SNAPSHOT: the ring is LIVE-TIMELINE state, NOT machine state. A COW fork
//! (Phase-2 `explore`) or a restored snapshot starts with a FRESH (empty) ring — so
//! `Clone` returns an empty ring of the same capacity (O(1), no 4 MiB copy), and the
//! ring is never serialized into a `.c64re` snapshot. History belongs to the run
//! that produced it, not to a past state you jumped back to.

/// One retired-instruction record. `#[repr(C)]` for a stable layout: u64 `cycle`
/// first (8-byte aligned), then the u16 `pc`, then the u8 run; the struct rounds up
/// to the u64 alignment = 24 bytes. (Packing it to 18 would need `#[repr(packed)]`,
/// which forbids references to the misaligned u64 — not worth it for 6 KiB/256-entry.)
///
/// Fields mirror the `Observer::on_instruction` arguments exactly (the same values
/// the trace `CPU_STEP` record carries): POST-instruction registers, the opcode-PC,
/// the opcode + its two raw operand bytes, and the CPU master `cycle` the trace
/// stamps (post-instruction clk minus the trailing tick — see `execute_one`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct CpuHistEntry {
    /// CPU master clock stamp (= trace CPU_STEP `cycle`).
    pub cycle: u64,
    /// Address of the instruction (= reg_pc at the opcode fetch).
    pub pc: u16,
    /// Opcode byte.
    pub opcode: u8,
    /// Raw operand byte 1 (0 for a 1-byte opcode).
    pub b1: u8,
    /// Raw operand byte 2 (0 for a 1/2-byte opcode).
    pub b2: u8,
    /// Accumulator AFTER the instruction.
    pub a: u8,
    /// X AFTER the instruction.
    pub x: u8,
    /// Y AFTER the instruction.
    pub y: u8,
    /// Stack pointer (low byte) AFTER the instruction.
    pub sp: u8,
    /// Processor status AFTER the instruction (B-flag masked out, = trace `p`).
    pub p: u8,
}

/// Default ring capacity (instructions). 256 Ki entries × 24 B = 6 MiB. ~0.25 s of
/// 6502 history at ~1 MHz (≈ a few PAL frames of instructions) — enough to cover a
/// crash run-up the user would `chis` back over, bounded in memory.
pub const DEFAULT_CAPACITY: usize = 256 * 1024;

/// Always-on circular CPU-history ring (VICE `cpuhistory`).
///
/// Layout: a flat slab of `capacity` slots + a monotonic `head` write counter. The
/// physical slot for the k-th pushed instruction is `k % capacity`; once `head`
/// exceeds `capacity` the oldest entries are overwritten in place. `len()` =
/// `min(head, capacity)`. No `Vec` growth, no per-push allocation.
pub struct CpuHistoryRing {
    /// Pre-allocated slab (built once; never resized). Boxed so the 4 MiB lives on
    /// the heap, not inline in `Machine`.
    slab: Box<[CpuHistEntry]>,
    /// Total instructions ever pushed (monotonic). The newest entry sits at
    /// `(head - 1) % capacity`; `head` never wraps (u64).
    head: u64,
    /// When false, `push` is a single early-return (kill-switch / not-yet-armed).
    enabled: bool,
}

impl CpuHistoryRing {
    /// Build a ring with `DEFAULT_CAPACITY`, honoring the `TRX64_CPUHISTORY` env
    /// kill-switch (default-ON; `0`/`off`/`false` ⇒ disabled).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Build a ring with an explicit capacity (≥ 1), honoring the env kill-switch.
    pub fn with_capacity(capacity: usize) -> Self {
        let cap = capacity.max(1);
        let enabled = match std::env::var("TRX64_CPUHISTORY") {
            Ok(v) => {
                let v = v.trim().to_ascii_lowercase();
                !(v == "0" || v == "off" || v == "false" || v == "no")
            }
            Err(_) => true,
        };
        Self {
            slab: vec![CpuHistEntry::default(); cap].into_boxed_slice(),
            head: 0,
            enabled,
        }
    }

    /// RUNTIME RESIZE (reverse-debug depth knob). Rebuild the slab at a new capacity
    /// for FUTURE capture and DROP all current history (the slab is freshly
    /// allocated). The `enabled` (kill-switch) state is preserved. NOT on the hot
    /// path — a deliberate `runtime/set_reverse_depth` / `revdepth` operation. It
    /// cannot retroactively extend history (a culprit already scrolled out is gone);
    /// only capture from now on uses the new capacity. Sibling of `DeltaRing::resize`.
    pub fn resize(&mut self, capacity: usize) {
        let cap = capacity.max(1);
        self.slab = vec![CpuHistEntry::default(); cap].into_boxed_slice();
        self.head = 0;
        // `enabled` deliberately preserved.
    }

    /// Ring capacity (max retained instructions).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.slab.len()
    }

    /// Number of valid entries currently retained (`min(head, capacity)`).
    #[inline]
    pub fn len(&self) -> usize {
        (self.head as usize).min(self.slab.len())
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head == 0
    }

    /// Whether the kill-switch left the ring armed.
    #[inline]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Force the armed state (test hook + future JAM-arm coupling).
    #[inline]
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// HOT PATH. Record one retired instruction. A few field stores + an index
    /// advance; overwrites the oldest slot once full. Zero allocation.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn push(
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
        cycle: u64,
    ) {
        if !self.enabled {
            return;
        }
        let cap = self.slab.len();
        let slot = (self.head % cap as u64) as usize;
        // Single struct store into the pre-allocated slot (no bounds-check escape
        // hatch needed: `slot < cap` by construction).
        self.slab[slot] = CpuHistEntry { cycle, pc, opcode, b1, b2, a, x, y, sp, p };
        self.head = self.head.wrapping_add(1);
    }

    /// Clear the ring (drop all history) without freeing the slab. Used on a cold
    /// reset / media swap so `chis` never shows pre-boundary instructions as if
    /// continuous (the trace boundary the spec says to report, not fake).
    #[inline]
    pub fn clear(&mut self) {
        self.head = 0;
    }

    /// Copy the last `n` entries (oldest → newest) into `out`. Clears `out` first.
    /// Caps at `len()`. Cheap (a couple of `extend_from_slice`s); NOT on the hot
    /// path — called only by `chis`.
    pub fn last_n(&self, n: usize, out: &mut Vec<CpuHistEntry>) {
        out.clear();
        let len = self.len();
        let take = n.min(len);
        if take == 0 {
            return;
        }
        let cap = self.slab.len();
        // The newest entry is at (head-1) % cap; the window of `take` ends there.
        // first_logical = head - take .. head, mapped through % cap.
        let start = self.head - take as u64;
        for k in 0..take {
            let slot = ((start + k as u64) % cap as u64) as usize;
            out.push(self.slab[slot]);
        }
    }

    /// Copy entries whose `cycle` is in `[lo, hi]` (inclusive), oldest → newest,
    /// into `out`. Clears `out` first. The ring is push-ordered by cycle (the CPU
    /// clock is monotonic per Spec 743), so this is a forward scan of the live
    /// window. Returns the count.
    pub fn window_by_cycle(&self, lo: u64, hi: u64, out: &mut Vec<CpuHistEntry>) -> usize {
        out.clear();
        let len = self.len();
        if len == 0 {
            return 0;
        }
        let cap = self.slab.len();
        let start = self.head - len as u64;
        for k in 0..len {
            let slot = ((start + k as u64) % cap as u64) as usize;
            let e = self.slab[slot];
            if e.cycle >= lo && e.cycle <= hi {
                out.push(e);
            }
        }
        out.len()
    }

    /// The cycle range currently covered by the ring (`Some((oldest, newest))`), or
    /// `None` when empty. Lets `chis` decide ring-vs-trace for an explicit window:
    /// if the requested window is older than `oldest`, fall back to the trace.
    pub fn cycle_span(&self) -> Option<(u64, u64)> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let cap = self.slab.len();
        let oldest_slot = ((self.head - len as u64) % cap as u64) as usize;
        let newest_slot = ((self.head - 1) % cap as u64) as usize;
        Some((self.slab[oldest_slot].cycle, self.slab[newest_slot].cycle))
    }
}

impl Default for CpuHistoryRing {
    fn default() -> Self {
        Self::new()
    }
}

/// CLONE = a FRESH, EMPTY ring of the same capacity + armed state. The ring is
/// live-timeline state, not machine state: a COW fork / restored snapshot has its
/// OWN forward history, so it must NOT inherit (and pay 4 MiB to copy) the parent's
/// instruction log. O(1) allocation of a zeroed slab.
impl Clone for CpuHistoryRing {
    fn clone(&self) -> Self {
        Self {
            slab: vec![CpuHistEntry::default(); self.slab.len()].into_boxed_slice(),
            head: 0,
            enabled: self.enabled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(pc: u16, cycle: u64) -> CpuHistEntry {
        CpuHistEntry { pc, cycle, opcode: (pc & 0xff) as u8, ..Default::default() }
    }

    fn push_ent(r: &mut CpuHistoryRing, e: CpuHistEntry) {
        r.push(e.pc, e.opcode, e.b1, e.b2, e.a, e.x, e.y, e.sp, e.p, e.cycle);
    }

    #[test]
    fn push_and_read_back_order_and_fields() {
        let mut r = CpuHistoryRing::with_capacity(8);
        r.set_enabled(true);
        // Push K=5 < capacity. last_n must return them oldest→newest, fields exact.
        for i in 0..5u64 {
            r.push(0x1000 + i as u16, 0xa9, 0x42, 0, (i + 1) as u8, 2, 3, 0xf8, 0x30, 100 + i);
        }
        assert_eq!(r.len(), 5);
        let mut out = Vec::new();
        r.last_n(5, &mut out);
        assert_eq!(out.len(), 5);
        for (i, e) in out.iter().enumerate() {
            assert_eq!(e.pc, 0x1000 + i as u16, "pc order");
            assert_eq!(e.cycle, 100 + i as u64, "cycle order");
            assert_eq!(e.opcode, 0xa9);
            assert_eq!(e.b1, 0x42);
            assert_eq!(e.a, (i + 1) as u8, "a field exact");
            assert_eq!(e.sp, 0xf8);
            assert_eq!(e.p, 0x30);
        }
        // last_n with n > len caps at len.
        let mut out2 = Vec::new();
        r.last_n(99, &mut out2);
        assert_eq!(out2.len(), 5);
    }

    #[test]
    fn wraps_correctly_at_capacity() {
        let cap = 8;
        let mut r = CpuHistoryRing::with_capacity(cap);
        r.set_enabled(true);
        // Push 20 = 2.5× capacity. Only the last `cap` survive, in order.
        for i in 0..20u64 {
            push_ent(&mut r, ent(0x2000 + i as u16, 500 + i));
        }
        assert_eq!(r.len(), cap, "len caps at capacity after wrap");
        let mut out = Vec::new();
        r.last_n(cap, &mut out);
        assert_eq!(out.len(), cap);
        // The surviving window is the LAST `cap` pushes: i = 12..20.
        for (k, e) in out.iter().enumerate() {
            let i = 12 + k as u64;
            assert_eq!(e.pc, 0x2000 + i as u16, "wrapped pc order");
            assert_eq!(e.cycle, 500 + i, "wrapped cycle order");
        }
        // The newest is the very last push.
        assert_eq!(out.last().unwrap().pc, 0x2000 + 19);
    }

    #[test]
    fn cycle_span_and_window() {
        let mut r = CpuHistoryRing::with_capacity(16);
        r.set_enabled(true);
        for i in 0..10u64 {
            push_ent(&mut r, ent(0x3000 + i as u16, 1000 + i * 10));
        }
        assert_eq!(r.cycle_span(), Some((1000, 1090)));
        let mut out = Vec::new();
        // Inclusive window [1020, 1050] → cycles 1020,1030,1040,1050 = 4 entries.
        let n = r.window_by_cycle(1020, 1050, &mut out);
        assert_eq!(n, 4);
        assert_eq!(out.first().unwrap().cycle, 1020);
        assert_eq!(out.last().unwrap().cycle, 1050);
    }

    #[test]
    fn disabled_records_nothing() {
        let mut r = CpuHistoryRing::with_capacity(8);
        r.set_enabled(false);
        for i in 0..5u64 {
            push_ent(&mut r, ent(i as u16, i));
        }
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
    }

    #[test]
    fn clear_drops_history_keeps_capacity() {
        let mut r = CpuHistoryRing::with_capacity(8);
        r.set_enabled(true);
        for i in 0..5u64 {
            push_ent(&mut r, ent(i as u16, i));
        }
        assert_eq!(r.len(), 5);
        r.clear();
        assert_eq!(r.len(), 0);
        assert_eq!(r.capacity(), 8);
        // Still usable after clear.
        push_ent(&mut r, ent(0x9, 9));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn resize_rebuilds_slab_drops_history_keeps_enabled() {
        // Reverse-depth knob sibling of DeltaRing::resize.
        let mut r = CpuHistoryRing::with_capacity(8);
        r.set_enabled(true);
        for i in 0..6u64 {
            push_ent(&mut r, ent(0x40 + i as u16, i));
        }
        assert_eq!(r.len(), 6);
        // GROW → history dropped, new capacity, still armed.
        r.resize(32);
        assert_eq!(r.capacity(), 32);
        assert_eq!(r.len(), 0, "resize drops history");
        assert!(r.enabled(), "resize keeps the armed flag");
        for i in 0..20u64 {
            push_ent(&mut r, ent(0x50 + i as u16, 100 + i));
        }
        assert_eq!(r.len(), 20, "grown ring retains the 20 (< 32 cap)");
        // SHRINK → caps at the new size.
        r.resize(4);
        assert_eq!(r.capacity(), 4);
        for i in 0..10u64 {
            push_ent(&mut r, ent(0x60 + i as u16, 200 + i));
        }
        assert_eq!(r.len(), 4, "shrunk ring caps at the NEW capacity");
        // Disabled stays disabled across resize.
        let mut d = CpuHistoryRing::with_capacity(8);
        d.set_enabled(false);
        d.resize(4);
        assert!(!d.enabled());
        push_ent(&mut d, ent(1, 1));
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn clone_is_empty_same_capacity() {
        let mut r = CpuHistoryRing::with_capacity(32);
        r.set_enabled(true);
        for i in 0..10u64 {
            push_ent(&mut r, ent(i as u16, i));
        }
        let c = r.clone();
        assert_eq!(c.capacity(), 32, "clone keeps capacity");
        assert_eq!(c.len(), 0, "clone starts empty (live-timeline, not machine state)");
        assert!(c.enabled(), "clone keeps armed state");
    }

    #[test]
    fn entry_is_24_bytes_bounded() {
        // 24 B × 256 Ki = 6 MiB ring — bounded. (u64 cycle forces the 8-byte align.)
        assert_eq!(std::mem::size_of::<CpuHistEntry>(), 24, "entry layout drifted (bound check)");
    }
}
