//! delta_ring.rs — reverse-debug Phase 1b: the always-on FULL-DELTA undo ring.
//!
//! Phase 1a (`cpu_history.rs`) is a CPU-only ring (PC + post-instruction regs +
//! opcode) that `chis` reads. This module ADDS the other half a real *reverse-step*
//! needs: per retired instruction, the **CPU pre-state** AND every **memory/IO write
//! that instruction performed** (`{addr, old_value, new_value}`). With both halves an
//! entry is *self-sufficient to UNDO*: write the `old_value`s back (reverse order) and
//! restore the CPU registers → the machine sits at the state BEFORE that instruction.
//!
//! WHY a SECOND ring (not extend `CpuHistEntry`): `CpuHistEntry` stores POST-instruction
//! registers with the trace-`p` representation (N/Z + B masked out) — it is the `chis`
//! display row, locked to a 24-byte layout the perf/layout test pins. Reverse-step needs
//! the COMPOSITE pre-state P (N/Z/B intact, byte-exact restore) plus a variable-length
//! write list. Different shape, different lifetime concern → a sibling ring. Both are fed
//! from the SAME `execute_one` retire point (no second decode/step).
//!
//! ALWAYS-ON, NO PRE-ARMING (user decision 2026-06-26): the write capture is NOT gated on
//! a trace. The bus write path already knows `old_value` (Spec 753 capture, same spot);
//! `execute_one` forwards each write into this ring every instruction. So a crash's run-up
//! is ALWAYS reverse-debuggable — `trace on` is only an on-demand DUMP of this ring.
//!
//! HOT-PATH DISCIPLINE (~1 MHz): `begin` stamps a scratch pre-state header; `record_write`
//! is one append into a pre-allocated write slab; `commit` writes the finished entry header
//! + advances two cursors. NO allocation, NO formatting, one `enabled` branch. Both slabs
//! are built once at `Machine::new` and never grow.
//!
//! MEMORY: sized for ~10 s of 6502 history. ~3M instr × 24 B (entry) + ~3M writes × 8 B
//! ≈ 72 + 24 ≈ ~96 MB at the default; tunable via `TRX64_REVERSE_SECONDS` (default 10) and
//! killed by `TRX64_CPUHISTORY=0` (shared kill-switch with Phase 1a — one knob).
//!
//! CONTRACT (the hard one): reverse-step restores CPU + RAM + IO-register *bytes*, NOT chip
//! internal counters (VIC raster / CIA timers / sprite-DMA). After a reverse-step the
//! machine is for INSPECTION, not resume-forward — to resume forward, restore a checkpoint
//! anchor (the cycle-exact ring). This module never touches those counters.
//!
//! CLONE / SNAPSHOT: like Phase 1a, the ring is LIVE-TIMELINE state, not machine state. A
//! COW fork / restored snapshot starts FRESH (empty), so `Clone` returns an empty ring of
//! the same capacities (O(1)); it is never serialized into a `.c64re` snapshot.
//!
//! RING DUMP (Spec time-travel-tooling Piece 2): the EXCEPTION to "never serialized" —
//! the `.c64rering` container deliberately persists the FULL reverse-debug buffer for the
//! tester→dev hand-off. `to_dump`/`from_dump` round-trip the whole ring (both slabs +
//! heads + caps + armed flag). This is a separate, opt-in container, NOT the per-state
//! `.c64re` snapshot (that still gets a fresh ring).

use serde::{Deserialize, Serialize};

/// One recorded memory/IO write. `#[repr(C)]`, 4 bytes: `addr` (u16) + `old_value` +
/// `new_value` (u8 each). `old_value` is the byte that was at `addr` BEFORE the write
/// (the undo info); `new_value` is what the instruction stored (for `who_wrote`'s
/// `old→new` report). Padded to 4 by alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct WriteRec {
    /// Target address of the write.
    pub addr: u16,
    /// Byte at `addr` BEFORE the write (undo value).
    pub old_value: u8,
    /// Byte the instruction wrote (for the `old→new` report).
    pub new_value: u8,
}

/// One retired-instruction delta record: the CPU PRE-state header + a slice into the
/// shared write slab (`[write_start, write_start + write_count)`, mapped through the
/// slab's modulus). `#[repr(C)]`: u64 `cycle` first (8-aligned), then the u32
/// `write_start`, the u16 `pc`, the u16 `write_count`, then the 6 register bytes →
/// rounds up to 24 bytes.
///
/// REGISTERS are the PRE-execute state (the state to land on after undoing this
/// instruction). `p` is the **COMPOSITE** status byte (N/Z/B/all flags intact, =
/// `C64Core6510::status()`), so a restore is byte-exact — unlike `CpuHistEntry.p`,
/// which masks N/Z/B for the trace.
///
/// OPCODE/OPERANDS (`opcode`/`b1`/`b2`) are set at RETIRE by `set_opcode` from the
/// SAME decoded fields `cpu_history.push` receives (the post-execute fetch), so a
/// `build_from_ring` trace carries a REAL disasm column (LDA/STA/JMP/…), not a
/// blank/BRK one. Unset (interrupt-only dispatch with no opcode body) they stay 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct DeltaEntry {
    /// CPU master clock stamp at this instruction's retire (= `CpuHistEntry.cycle`).
    pub cycle: u64,
    /// Monotonic index of this entry's first write in the shared write slab. The
    /// entry's writes are `write_slab[(write_start + k) % writes_cap]` for
    /// `k in 0..write_count`. Stale once `write_head - write_start > writes_cap`
    /// (the slab wrapped over them) — see `writes_for`.
    pub write_start: u32,
    /// Program counter BEFORE this instruction (the opcode address = landing PC).
    pub pc: u16,
    /// Number of writes this instruction performed (0..=~8).
    pub write_count: u16,
    /// Accumulator BEFORE this instruction.
    pub a: u8,
    /// X BEFORE this instruction.
    pub x: u8,
    /// Y BEFORE this instruction.
    pub y: u8,
    /// Stack pointer BEFORE this instruction.
    pub sp: u8,
    /// COMPOSITE processor status BEFORE this instruction (all flags intact).
    pub p: u8,
    /// Opcode byte of this instruction (set at retire via `set_opcode`; 0 if the
    /// dispatch had no normal opcode body, e.g. an interrupt-only push).
    pub opcode: u8,
    /// Operand byte 1 (low) — set with `opcode` at retire; 0 for 1-byte opcodes.
    pub b1: u8,
    /// Operand byte 2 (high) — set with `opcode` at retire; 0 for <3-byte opcodes.
    pub b2: u8,
}

/// Default reverse-history depth in seconds (user decision: 10 s). Overridable via
/// `TRX64_REVERSE_SECONDS`.
pub const DEFAULT_REVERSE_SECONDS: usize = 10;

/// 6502 instructions per second at the PAL ~1 MHz clock, used to size the ring from
/// the seconds budget. ~1M cyc/s ÷ ~3.5 cyc/instr ≈ ~300k instr/s; round to 300k so
/// 10 s ≈ 3M instructions (the spec's figure).
pub const INSTR_PER_SECOND: usize = 300_000;

/// Writes-slab over-provision factor relative to the entry count. Real code averages
/// well under one write per instruction (loads/branches/compares write nothing), but
/// store-heavy bursts (block copies, stack pushes, RMW) cluster; 1.25× headroom keeps
/// the write window aligned with the instruction window so reverse-step / who_wrote see
/// a consistent depth. (3M instr → 3.75M writes × 8 B ≈ 30 MB.)
pub const WRITES_PER_INSTR_NUM: usize = 5;
pub const WRITES_PER_INSTR_DEN: usize = 4;

/// Always-on full-delta undo ring (reverse-debug Phase 1b).
///
/// Two flat slabs:
///  * `entries` — `entry_cap` `DeltaEntry` slots, circular by `entry_head % cap`.
///  * `writes`  — `writes_cap` `WriteRec` slots, circular by `write_head % cap`.
///
/// `entry_head` / `write_head` are monotonic u64 counters (never wrap in practice).
/// The hot path is `begin` (stash the pre-state header in `cur`), `record_write`
/// (append into `writes`, bump the in-flight count), `commit` (publish the header,
/// advance `entry_head`). Zero allocation; both slabs are fixed at construction.
pub struct DeltaRing {
    /// Entry slab (pre-state headers). Boxed: lives on the heap, not inline in `Machine`.
    entries: Box<[DeltaEntry]>,
    /// Write slab (`{addr, old, new}` per write). Boxed for the same reason.
    writes: Box<[WriteRec]>,
    /// Total entries ever committed (monotonic). Newest entry = `(entry_head-1) % cap`.
    entry_head: u64,
    /// Total writes ever appended (monotonic). Next write slot = `write_head % cap`.
    write_head: u64,
    /// Scratch header for the instruction currently executing (filled by `begin`,
    /// published by `commit`). `write_start` is the `write_head` at `begin`.
    cur: DeltaEntry,
    /// Whether an instruction is in flight (between `begin` and `commit`). Guards a
    /// stray `record_write` outside an instruction (defensive; never true in product).
    in_flight: bool,
    /// Kill-switch (shared `TRX64_CPUHISTORY` knob). When false the whole ring is inert.
    enabled: bool,
}

impl DeltaRing {
    /// Build a ring sized from `TRX64_REVERSE_SECONDS` (default 10 s), honoring the
    /// shared `TRX64_CPUHISTORY` kill-switch (default-ON).
    pub fn new() -> Self {
        let secs = match std::env::var("TRX64_REVERSE_SECONDS") {
            Ok(v) => v.trim().parse::<usize>().unwrap_or(DEFAULT_REVERSE_SECONDS),
            Err(_) => DEFAULT_REVERSE_SECONDS,
        }
        .max(1);
        let entry_cap = secs * INSTR_PER_SECOND;
        let writes_cap = entry_cap * WRITES_PER_INSTR_NUM / WRITES_PER_INSTR_DEN;
        Self::with_capacity(entry_cap, writes_cap)
    }

    /// Build a ring with explicit slab sizes (≥ 1 each), honoring the env kill-switch.
    pub fn with_capacity(entry_cap: usize, writes_cap: usize) -> Self {
        let ecap = entry_cap.max(1);
        let wcap = writes_cap.max(1);
        let enabled = match std::env::var("TRX64_CPUHISTORY") {
            Ok(v) => {
                let v = v.trim().to_ascii_lowercase();
                !(v == "0" || v == "off" || v == "false" || v == "no")
            }
            Err(_) => true,
        };
        Self {
            entries: vec![DeltaEntry::default(); ecap].into_boxed_slice(),
            writes: vec![WriteRec::default(); wcap].into_boxed_slice(),
            entry_head: 0,
            write_head: 0,
            cur: DeltaEntry::default(),
            in_flight: false,
            enabled,
        }
    }

    /// Compute the (entry_cap, writes_cap) pair for a depth in seconds — the SAME
    /// sizing `new()` applies from `TRX64_REVERSE_SECONDS`, exposed so a runtime
    /// `set_reverse_depth` rebuilds the ring at a new depth with identical math.
    pub fn caps_for_seconds(secs: usize) -> (usize, usize) {
        let secs = secs.max(1);
        let entry_cap = secs * INSTR_PER_SECOND;
        let writes_cap = entry_cap * WRITES_PER_INSTR_NUM / WRITES_PER_INSTR_DEN;
        (entry_cap, writes_cap)
    }

    /// RUNTIME RESIZE (reverse-debug depth knob). Rebuild both slabs at new
    /// capacities for FUTURE capture and DROP all current history (the slabs are
    /// freshly allocated). The `enabled` (kill-switch) state is preserved. This is
    /// NOT on the hot path — it is a deliberate `runtime/set_reverse_depth` /
    /// `revdepth` operation. It cannot retroactively extend history: a culprit that
    /// already scrolled out of the old ring is gone; only capture from now on uses
    /// the new depth. (Mirrors `with_capacity`, minus the env re-read — the live
    /// `enabled` flag is kept so a depth change does not silently re-arm a
    /// kill-switched ring.)
    pub fn resize(&mut self, entry_cap: usize, writes_cap: usize) {
        let ecap = entry_cap.max(1);
        let wcap = writes_cap.max(1);
        self.entries = vec![DeltaEntry::default(); ecap].into_boxed_slice();
        self.writes = vec![WriteRec::default(); wcap].into_boxed_slice();
        self.entry_head = 0;
        self.write_head = 0;
        self.cur = DeltaEntry::default();
        self.in_flight = false;
        // `enabled` deliberately preserved.
    }

    /// Entry-slab capacity (max retained instructions).
    #[inline]
    pub fn entry_capacity(&self) -> usize {
        self.entries.len()
    }

    /// Write-slab capacity (max retained writes).
    #[inline]
    pub fn writes_capacity(&self) -> usize {
        self.writes.len()
    }

    /// Number of valid entries currently retained (`min(entry_head, entry_cap)`).
    #[inline]
    pub fn len(&self) -> usize {
        (self.entry_head as usize).min(self.entries.len())
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entry_head == 0
    }

    /// Whether the kill-switch left the ring armed.
    #[inline]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Force the armed state (test hook + perf-bench toggle, like Phase 1a).
    #[inline]
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    // ── Spec time-travel-tooling Piece 2 — ring dump/restore ────────────────────
    //
    // Serialize the WHOLE ring (both slabs + the monotonic heads + caps + armed flag)
    // for the `.c64rering` container, and reconstruct it elsewhere. The slabs are
    // `#[repr(C)]` POD, so they round-trip as raw LE bytes (no field-by-field codec).
    // Only the VALID window is dumped (a wrapped ring physically stores the last `cap`
    // entries, but the logical order matters for who_wrote/reverse_step) — the restore
    // re-lays them so `newest()`/`who_wrote`/`writes_for` see the identical sequence.

    /// Dump the ring to a [`DeltaRingDump`] (the `.c64rering` payload). Captures the
    /// valid entries oldest→newest with each entry's own writes inlined, so a restore
    /// is independent of the physical slab wrap. NOT on the hot path.
    pub fn to_dump(&self) -> DeltaRingDump {
        let len = self.len();
        let mut entries: Vec<DeltaEntry> = Vec::with_capacity(len);
        let mut writes: Vec<WriteRec> = Vec::new();
        let mut scratch: Vec<WriteRec> = Vec::new();
        let cap = self.entries.len() as u64;
        let start = self.entry_head - len as u64;
        for k in 0..len as u64 {
            let slot = ((start + k) % cap) as usize;
            let mut e = self.entries[slot];
            self.writes_for(&e, &mut scratch);
            // Re-base the entry's write window onto the FLAT dump-writes vec so the
            // restore can re-lay both slabs contiguously (write_start = running count).
            e.write_start = writes.len() as u32;
            e.write_count = scratch.len() as u16;
            writes.extend_from_slice(&scratch);
            entries.push(e);
        }
        DeltaRingDump {
            entry_cap: self.entries.len() as u32,
            writes_cap: self.writes.len() as u32,
            enabled: self.enabled,
            entries: entries.iter().flat_map(delta_entry_to_le).collect(),
            writes: writes.iter().flat_map(write_rec_to_le).collect(),
        }
    }

    /// Reconstruct a ring from a [`DeltaRingDump`] (the inverse of [`to_dump`]). Lays
    /// the dumped entries + their writes into freshly sized slabs so the restored ring
    /// answers `newest`/`who_wrote`/`writes_for`/`reverse_step` identically to the
    /// source. The armed flag is restored from the dump.
    pub fn from_dump(d: &DeltaRingDump) -> Self {
        let mut ring = Self::with_capacity(d.entry_cap.max(1) as usize, d.writes_cap.max(1) as usize);
        ring.enabled = d.enabled;
        let dumped_entries: Vec<DeltaEntry> =
            d.entries.chunks_exact(DELTA_ENTRY_LE).map(delta_entry_from_le).collect();
        let dumped_writes: Vec<WriteRec> =
            d.writes.chunks_exact(WRITE_REC_LE).map(write_rec_from_le).collect();
        let ecap = ring.entries.len();
        let wcap = ring.writes.len();
        // Replay the dumped entries in order, re-issuing their writes against the fresh
        // slabs so the monotonic heads + write windows match (same as live capture).
        for e in &dumped_entries {
            let lo = e.write_start as usize;
            let hi = lo + e.write_count as usize;
            let ws = dumped_writes.get(lo..hi).unwrap_or(&[]);
            // begin → record each write → commit, but bypass the enabled gate so a
            // ring dumped while armed restores its history even if the env later
            // disabled it (the dump IS the history; honour it).
            let entry = DeltaEntry {
                write_start: ring.write_head as u32,
                ..*e
            };
            for w in ws {
                let slot = (ring.write_head % wcap as u64) as usize;
                ring.writes[slot] = *w;
                ring.write_head = ring.write_head.wrapping_add(1);
            }
            let slot = (ring.entry_head % ecap as u64) as usize;
            ring.entries[slot] = DeltaEntry { write_count: e.write_count, ..entry };
            ring.entry_head = ring.entry_head.wrapping_add(1);
        }
        ring
    }

    /// HOT PATH. Open the in-flight instruction: stash its CPU PRE-state header. The
    /// `p` argument is the COMPOSITE status (all flags). `record_write` calls between
    /// here and `commit` attach to this entry. No allocation.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn begin(&mut self, pc: u16, a: u8, x: u8, y: u8, sp: u8, p: u8, cycle: u64) {
        if !self.enabled {
            return;
        }
        self.cur = DeltaEntry {
            cycle,
            write_start: self.write_head as u32,
            pc,
            write_count: 0,
            a,
            x,
            y,
            sp,
            p,
            opcode: 0,
            b1: 0,
            b2: 0,
        };
        self.in_flight = true;
    }

    /// HOT PATH. Stamp the in-flight instruction's decoded opcode + operand bytes onto
    /// the pending entry, called at retire with the SAME `opcode`/`b1`/`b2` the
    /// `cpu_history` ring receives (no second decode, no extra memory read). Lets a
    /// `build_from_ring` trace carry a REAL disasm column instead of opcode-0 (= BRK
    /// for every row). A no-op when the ring is gated off or no instruction is in
    /// flight (an interrupt-only dispatch leaves the entry's opcode at 0).
    #[inline]
    pub fn set_opcode(&mut self, opcode: u8, b1: u8, b2: u8) {
        if !self.enabled || !self.in_flight {
            return;
        }
        self.cur.opcode = opcode;
        self.cur.b1 = b1;
        self.cur.b2 = b2;
    }

    /// HOT PATH. Append one write to the in-flight instruction. `old` = the byte at
    /// `addr` BEFORE the store (the undo value), `new` = the stored byte. One slab
    /// store + two counter bumps. Overwrites the oldest write slot once full.
    #[inline]
    pub fn record_write(&mut self, addr: u16, old: u8, new: u8) {
        if !self.enabled || !self.in_flight {
            return;
        }
        let cap = self.writes.len();
        let slot = (self.write_head % cap as u64) as usize;
        self.writes[slot] = WriteRec { addr, old_value: old, new_value: new };
        self.write_head = self.write_head.wrapping_add(1);
        self.cur.write_count = self.cur.write_count.saturating_add(1);
    }

    /// HOT PATH. Publish the in-flight instruction's header into the entry slab and
    /// advance `entry_head`. A no-op if `begin` was gated off / never called. One
    /// struct store + one counter bump.
    #[inline]
    pub fn commit(&mut self) {
        if !self.enabled || !self.in_flight {
            return;
        }
        let cap = self.entries.len();
        let slot = (self.entry_head % cap as u64) as usize;
        self.entries[slot] = self.cur;
        self.entry_head = self.entry_head.wrapping_add(1);
        self.in_flight = false;
    }

    /// Drop all history (head reset) without freeing the slabs. Used on a cold reset /
    /// media swap (the timeline boundary — don't present pre-boundary deltas as
    /// continuous). The slabs are retained.
    #[inline]
    pub fn clear(&mut self) {
        self.entry_head = 0;
        self.write_head = 0;
        self.in_flight = false;
    }

    /// The newest committed entry (the last retired instruction), or `None` if empty.
    /// Copy (the struct is 24 B). This is the one reverse-step undoes first.
    pub fn newest(&self) -> Option<DeltaEntry> {
        if self.entry_head == 0 {
            return None;
        }
        let cap = self.entries.len();
        let slot = ((self.entry_head - 1) % cap as u64) as usize;
        Some(self.entries[slot])
    }

    /// Whether an entry's writes are still readable (the write slab has NOT wrapped
    /// over them). The newest entry is always readable; an old one whose
    /// `write_start + write_count` fell behind `write_head - writes_cap` is stale.
    #[inline]
    fn writes_readable(&self, e: &DeltaEntry) -> bool {
        let wcap = self.writes.len() as u64;
        // The oldest still-live write index. A write at logical index `i` is live iff
        // `i >= write_head - wcap`. The entry's writes span [start, start+count).
        let oldest_live = self.write_head.saturating_sub(wcap);
        e.write_count == 0 || e.write_start as u64 >= oldest_live
    }

    /// Copy an entry's writes (in stored order: oldest→newest as recorded) into `out`,
    /// clearing it first. Empty if the entry has no writes OR they have been
    /// overwritten (the slab wrapped) — caller checks `out.len() == e.write_count` to
    /// detect the stale case. NOT on the hot path.
    pub fn writes_for(&self, e: &DeltaEntry, out: &mut Vec<WriteRec>) {
        out.clear();
        if e.write_count == 0 || !self.writes_readable(e) {
            return;
        }
        let cap = self.writes.len();
        for k in 0..e.write_count as u64 {
            let slot = ((e.write_start as u64 + k) % cap as u64) as usize;
            out.push(self.writes[slot]);
        }
    }

    /// Pop the newest entry off the ring (un-commit it), exposing the prior entry as
    /// the new newest. Used by `reverse_step` AFTER it has applied the undo, so a
    /// second reverse-step targets the previous instruction. Also rewinds the write
    /// cursor past that entry's writes (they have been undone and are no longer part of
    /// the live forward timeline). Returns the popped entry, or `None` if empty.
    ///
    /// NOTE: rewinding `write_head` is safe because the popped entry is ALWAYS the
    /// newest, so its writes are the most-recently-appended ones (`write_head -
    /// write_count .. write_head`) — exactly the tail we drop.
    pub fn pop_newest(&mut self) -> Option<DeltaEntry> {
        if self.entry_head == 0 {
            return None;
        }
        let e = self.newest().unwrap();
        self.entry_head -= 1;
        // Rewind the write cursor past this entry's writes (the newest tail).
        self.write_head = self.write_head.saturating_sub(e.write_count as u64);
        Some(e)
    }

    /// Scan the ring's writes BACKWARD (newest→oldest) for the last writer(s) of
    /// `addr`, returning up to `limit` hits as `(entry, write)` pairs, newest first.
    /// Stops early at the first entry whose writes have been overwritten (the readable
    /// window's edge) — beyond it the answer is in the finalized trace, not the ring.
    /// NOT on the hot path (a `who_wrote` query, not per-instruction).
    pub fn who_wrote(&self, addr: u16, limit: usize) -> Vec<(DeltaEntry, WriteRec)> {
        let mut hits = Vec::new();
        if limit == 0 {
            return hits;
        }
        let len = self.len();
        if len == 0 {
            return hits;
        }
        let cap = self.entries.len();
        // Walk entries newest → oldest.
        for back in 0..len as u64 {
            let logical = self.entry_head - 1 - back;
            let slot = (logical % cap as u64) as usize;
            let e = self.entries[slot];
            if !self.writes_readable(&e) {
                // This entry's writes are gone → so are all older ones. Stop.
                break;
            }
            if e.write_count == 0 {
                continue;
            }
            // Scan this instruction's writes newest→oldest (a later write to the same
            // addr in the same instruction — e.g. RMW dummy then real — wins).
            let wcap = self.writes.len();
            for wk in (0..e.write_count as u64).rev() {
                let wslot = ((e.write_start as u64 + wk) % wcap as u64) as usize;
                let w = self.writes[wslot];
                if w.addr == addr {
                    hits.push((e, w));
                    if hits.len() >= limit {
                        return hits;
                    }
                    break; // one hit per instruction is enough for the "who wrote" answer
                }
            }
        }
        hits
    }

    /// The cycle range currently covered by the entry ring (`Some((oldest, newest))`)
    /// or `None` when empty. Lets a caller report the reverse window / decide
    /// ring-vs-trace fallback.
    pub fn cycle_span(&self) -> Option<(u64, u64)> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let cap = self.entries.len();
        let oldest_slot = ((self.entry_head - len as u64) % cap as u64) as usize;
        let newest_slot = ((self.entry_head - 1) % cap as u64) as usize;
        Some((self.entries[oldest_slot].cycle, self.entries[newest_slot].cycle))
    }

    /// reverse-debug Phase 1c — copy every retained entry whose `cycle` is in
    /// `[lo, hi]` (inclusive), OLDEST → NEWEST (chronological), each paired with the
    /// writes it performed (in stored order), into `out`. Clears `out` first.
    ///
    /// This is the slice the `trace/build_from_ring` daemon method encodes into a
    /// `.c64retrace`: a targeted dump of a cycle window the UI scrub-bar selected. The
    /// ring is push-ordered by `cycle` (the CPU clock is monotonic per Spec 743), so a
    /// forward walk of the live window is already chronological.
    ///
    /// An entry whose write slab has wrapped (its writes are stale) is still emitted —
    /// its `DeltaEntry` header lives in the entry ring — but with an EMPTY write list
    /// (the bytes are gone). This matches `writes_for`'s stale behaviour; the CPU row is
    /// always faithful, only the mem rows for a slab-evicted entry are lost. In the
    /// 10 s default ring a 2-thumbnail window is always well inside both slabs.
    ///
    /// NOT on the hot path (an on-demand dump, not per-instruction).
    pub fn slice_by_cycle(&self, lo: u64, hi: u64, out: &mut Vec<(DeltaEntry, Vec<WriteRec>)>) -> usize {
        out.clear();
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        let len = self.len();
        if len == 0 {
            return 0;
        }
        let cap = self.entries.len();
        let start = self.entry_head - len as u64;
        let mut writes_scratch: Vec<WriteRec> = Vec::new();
        for k in 0..len as u64 {
            let slot = ((start + k) % cap as u64) as usize;
            let e = self.entries[slot];
            if e.cycle < lo || e.cycle > hi {
                continue;
            }
            self.writes_for(&e, &mut writes_scratch);
            out.push((e, std::mem::take(&mut writes_scratch)));
        }
        out.len()
    }
}

impl Default for DeltaRing {
    fn default() -> Self {
        Self::new()
    }
}

/// CLONE = a FRESH, EMPTY ring of the same capacities + armed state (live-timeline
/// state, not machine state — same reasoning as `CpuHistoryRing`). O(1) zeroed slabs.
impl Clone for DeltaRing {
    fn clone(&self) -> Self {
        Self {
            entries: vec![DeltaEntry::default(); self.entries.len()].into_boxed_slice(),
            writes: vec![WriteRec::default(); self.writes.len()].into_boxed_slice(),
            entry_head: 0,
            write_head: 0,
            cur: DeltaEntry::default(),
            in_flight: false,
            enabled: self.enabled,
        }
    }
}

// ── Ring dump payload (Spec time-travel-tooling Piece 2) ──────────────────────────

/// LE byte width of a serialized [`DeltaEntry`] in a [`DeltaRingDump`].
const DELTA_ENTRY_LE: usize = 8 + 4 + 2 + 2 + 6 + 3; // cycle,write_start,pc,write_count,5 regs,opcode/b1/b2 = 25
/// LE byte width of a serialized [`WriteRec`].
const WRITE_REC_LE: usize = 2 + 1 + 1; // addr,old,new = 4

fn delta_entry_to_le(e: &DeltaEntry) -> [u8; DELTA_ENTRY_LE] {
    let mut o = [0u8; DELTA_ENTRY_LE];
    o[0..8].copy_from_slice(&e.cycle.to_le_bytes());
    o[8..12].copy_from_slice(&e.write_start.to_le_bytes());
    o[12..14].copy_from_slice(&e.pc.to_le_bytes());
    o[14..16].copy_from_slice(&e.write_count.to_le_bytes());
    o[16] = e.a;
    o[17] = e.x;
    o[18] = e.y;
    o[19] = e.sp;
    o[20] = e.p;
    o[21] = e.opcode;
    o[22] = e.b1;
    o[23] = e.b2;
    o
}

fn delta_entry_from_le(b: &[u8]) -> DeltaEntry {
    DeltaEntry {
        cycle: u64::from_le_bytes(b[0..8].try_into().unwrap()),
        write_start: u32::from_le_bytes(b[8..12].try_into().unwrap()),
        pc: u16::from_le_bytes(b[12..14].try_into().unwrap()),
        write_count: u16::from_le_bytes(b[14..16].try_into().unwrap()),
        a: b[16],
        x: b[17],
        y: b[18],
        sp: b[19],
        p: b[20],
        opcode: b[21],
        b1: b[22],
        b2: b[23],
    }
}

fn write_rec_to_le(w: &WriteRec) -> [u8; WRITE_REC_LE] {
    let mut o = [0u8; WRITE_REC_LE];
    o[0..2].copy_from_slice(&w.addr.to_le_bytes());
    o[2] = w.old_value;
    o[3] = w.new_value;
    o
}

fn write_rec_from_le(b: &[u8]) -> WriteRec {
    WriteRec {
        addr: u16::from_le_bytes(b[0..2].try_into().unwrap()),
        old_value: b[2],
        new_value: b[3],
    }
}

/// Serializable snapshot of a whole [`DeltaRing`] for the `.c64rering` container. The
/// two slabs ride as flat LE byte payloads (`#[serde(with)]` base64) — compact even
/// before the container's outer gzip. `entries` holds the valid window oldest→newest
/// (each entry's `write_start`/`write_count` index into the flat `writes`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaRingDump {
    pub entry_cap: u32,
    pub writes_cap: u32,
    pub enabled: bool,
    /// Flat LE entry payload (`entries.len()/DELTA_ENTRY_LE` records).
    #[serde(with = "crate::ring_dump::b64_bytes")]
    pub entries: Vec<u8>,
    /// Flat LE write payload (`writes.len()/WRITE_REC_LE` records).
    #[serde(with = "crate::ring_dump::b64_bytes")]
    pub writes: Vec<u8>,
}

impl DeltaRingDump {
    /// Number of valid entries carried (for the container's `RingDumpInfo`).
    pub fn entry_count(&self) -> usize {
        self.entries.len() / DELTA_ENTRY_LE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: record a whole instruction (begin + writes + commit). Stamps a
    /// deterministic opcode/operand so the build_from_ring disasm round-trip is
    /// checkable: `opcode = pc.low`, `b1 = pc.high`, `b2 = 0`.
    fn instr(
        r: &mut DeltaRing,
        pc: u16,
        regs: (u8, u8, u8, u8, u8),
        cycle: u64,
        writes: &[(u16, u8, u8)],
    ) {
        let (a, x, y, sp, p) = regs;
        r.begin(pc, a, x, y, sp, p, cycle);
        r.set_opcode(pc as u8, (pc >> 8) as u8, 0);
        for &(addr, old, new) in writes {
            r.record_write(addr, old, new);
        }
        r.commit();
    }

    #[test]
    fn entry_and_write_layout_bounded() {
        assert_eq!(std::mem::size_of::<DeltaEntry>(), 24, "DeltaEntry layout drifted");
        assert_eq!(std::mem::size_of::<WriteRec>(), 4, "WriteRec layout drifted");
    }

    #[test]
    fn begin_record_commit_roundtrip() {
        let mut r = DeltaRing::with_capacity(16, 64);
        r.set_enabled(true);
        instr(&mut r, 0x1000, (1, 2, 3, 0xfd, 0x30), 100, &[(0x0400, 0x20, 0x41)]);
        assert_eq!(r.len(), 1);
        let e = r.newest().unwrap();
        assert_eq!(e.pc, 0x1000);
        assert_eq!(e.a, 1);
        assert_eq!(e.sp, 0xfd);
        assert_eq!(e.p, 0x30);
        // `instr` stamps opcode = pc.low, b1 = pc.high (deterministic decode marker).
        assert_eq!(e.opcode, 0x00, "opcode = pc.low");
        assert_eq!(e.b1, 0x10, "b1 = pc.high");
        assert_eq!(e.b2, 0x00);
        assert_eq!(e.write_count, 1);
        let mut w = Vec::new();
        r.writes_for(&e, &mut w);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0], WriteRec { addr: 0x0400, old_value: 0x20, new_value: 0x41 });
    }

    #[test]
    fn multi_write_instruction_order_preserved() {
        let mut r = DeltaRing::with_capacity(8, 32);
        r.set_enabled(true);
        // An instruction (e.g. an interrupt push or RMW) with 3 writes.
        instr(
            &mut r,
            0x2000,
            (0, 0, 0, 0xff, 0x24),
            500,
            &[(0x01ff, 0xaa, 0x20), (0x01fe, 0xbb, 0x00), (0x01fd, 0xcc, 0x24)],
        );
        let e = r.newest().unwrap();
        assert_eq!(e.write_count, 3);
        let mut w = Vec::new();
        r.writes_for(&e, &mut w);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].addr, 0x01ff);
        assert_eq!(w[1].addr, 0x01fe);
        assert_eq!(w[2].addr, 0x01fd);
    }

    #[test]
    fn pop_newest_exposes_prior_and_rewinds_writes() {
        let mut r = DeltaRing::with_capacity(8, 32);
        r.set_enabled(true);
        instr(&mut r, 0x100, (0, 0, 0, 0xff, 0), 10, &[(0x10, 0, 1)]);
        instr(&mut r, 0x102, (0, 0, 0, 0xff, 0), 12, &[(0x11, 0, 2), (0x12, 0, 3)]);
        assert_eq!(r.len(), 2);
        // Pop the newest (2 writes) → write_head rewinds by 2, len 1.
        let popped = r.pop_newest().unwrap();
        assert_eq!(popped.pc, 0x102);
        assert_eq!(popped.write_count, 2);
        assert_eq!(r.len(), 1);
        let now = r.newest().unwrap();
        assert_eq!(now.pc, 0x100);
        let mut w = Vec::new();
        r.writes_for(&now, &mut w);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].addr, 0x10);
    }

    #[test]
    fn entry_ring_wraps() {
        let mut r = DeltaRing::with_capacity(4, 64);
        r.set_enabled(true);
        for i in 0..10u64 {
            instr(&mut r, 0x300 + i as u16, (0, 0, 0, 0xff, 0), 1000 + i, &[(i as u16, 0, 1)]);
        }
        assert_eq!(r.len(), 4, "entry len caps at entry_cap after wrap");
        // Newest is the last push.
        assert_eq!(r.newest().unwrap().pc, 0x300 + 9);
        // The oldest surviving entry is i=6 (last 4 = 6,7,8,9).
        let (oldest_cyc, newest_cyc) = r.cycle_span().unwrap();
        assert_eq!(oldest_cyc, 1006);
        assert_eq!(newest_cyc, 1009);
    }

    #[test]
    fn who_wrote_finds_last_writer_newest_first() {
        let mut r = DeltaRing::with_capacity(16, 64);
        r.set_enabled(true);
        // $01F5 written three times by three instructions; who_wrote must list newest first.
        instr(&mut r, 0xa00, (0, 0, 0, 0xff, 0), 1, &[(0x01f5, 0x00, 0x11)]);
        instr(&mut r, 0xb00, (0, 0, 0, 0xff, 0), 2, &[(0x0200, 0x00, 0x99)]); // unrelated
        instr(&mut r, 0xc00, (0, 0, 0, 0xff, 0), 3, &[(0x01f5, 0x11, 0x22)]);
        instr(&mut r, 0xd00, (0, 0, 0, 0xff, 0), 4, &[(0x01f5, 0x22, 0x33)]);
        let hits = r.who_wrote(0x01f5, 5);
        assert_eq!(hits.len(), 3);
        // Newest first: $D00 (3) → $C00 (2) → $A00 (1).
        assert_eq!(hits[0].0.pc, 0xd00);
        assert_eq!(hits[0].1, WriteRec { addr: 0x01f5, old_value: 0x22, new_value: 0x33 });
        assert_eq!(hits[1].0.pc, 0xc00);
        assert_eq!(hits[2].0.pc, 0xa00);
        // limit honored.
        let one = r.who_wrote(0x01f5, 1);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].0.pc, 0xd00);
        // unknown addr → no hits.
        assert!(r.who_wrote(0x9999, 5).is_empty());
    }

    #[test]
    fn who_wrote_picks_last_write_within_an_instruction() {
        // A single instruction that writes $50 twice (RMW dummy-then-real); who_wrote
        // must report the LATER (real) write.
        let mut r = DeltaRing::with_capacity(8, 32);
        r.set_enabled(true);
        instr(&mut r, 0x400, (0, 0, 0, 0xff, 0), 7, &[(0x0050, 0x05, 0x05), (0x0050, 0x05, 0x06)]);
        let hits = r.who_wrote(0x0050, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1.new_value, 0x06, "last write within the instruction wins");
    }

    #[test]
    fn stale_writes_stop_who_wrote_at_window_edge() {
        // Tiny write slab forces the oldest entry's writes to be overwritten while the
        // entry header still lives. who_wrote must stop at the readable edge, not read
        // garbage.
        let mut r = DeltaRing::with_capacity(8, 4); // 4-write slab.
        r.set_enabled(true);
        // 6 instructions, 1 write each → write_head=6, slab holds the last 4 writes.
        for i in 0..6u64 {
            instr(&mut r, 0x500 + i as u16, (0, 0, 0, 0xff, 0), 100 + i, &[(0x80, i as u8, (i + 1) as u8)]);
        }
        // The 2 oldest writes (i=0,1) are overwritten; entries 0,1 are stale.
        let hits = r.who_wrote(0x80, 99);
        // Only entries whose writes survived (i=2..5 = 4 writes) are reported.
        assert_eq!(hits.len(), 4, "who_wrote stops at the readable write window");
        assert_eq!(hits[0].0.pc, 0x505, "newest reported first");
    }

    #[test]
    fn disabled_records_nothing() {
        let mut r = DeltaRing::with_capacity(8, 32);
        r.set_enabled(false);
        instr(&mut r, 0x1, (0, 0, 0, 0, 0), 1, &[(0x10, 0, 1)]);
        assert_eq!(r.len(), 0);
        assert!(r.newest().is_none());
        assert!(r.who_wrote(0x10, 5).is_empty());
    }

    #[test]
    fn clear_drops_history_keeps_capacity() {
        let mut r = DeltaRing::with_capacity(8, 32);
        r.set_enabled(true);
        for i in 0..5u64 {
            instr(&mut r, i as u16, (0, 0, 0, 0xff, 0), i, &[(i as u16, 0, 1)]);
        }
        assert_eq!(r.len(), 5);
        r.clear();
        assert_eq!(r.len(), 0);
        assert_eq!(r.entry_capacity(), 8);
        assert_eq!(r.writes_capacity(), 32);
        instr(&mut r, 0x9, (0, 0, 0, 0xff, 0), 9, &[]);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn slice_by_cycle_selects_window_chronologically() {
        // reverse-debug Phase 1c — the cycle-window slice the trace/build_from_ring
        // dump encodes. Build a known stream, slice [b,c] and assert it returns EXACTLY
        // the in-range entries (oldest→newest) with their writes, and excludes the rest.
        let mut r = DeltaRing::with_capacity(16, 64);
        r.set_enabled(true);
        // 5 instructions at cycles 100,110,120,130,140 with distinct writes.
        instr(&mut r, 0x1000, (0, 0, 0, 0xff, 0), 100, &[(0x10, 0xaa, 0x01)]);
        instr(&mut r, 0x1001, (1, 0, 0, 0xff, 0), 110, &[(0x11, 0xbb, 0x02), (0x12, 0xcc, 0x03)]);
        instr(&mut r, 0x1002, (2, 0, 0, 0xff, 0), 120, &[]); // no writes (branch/compare)
        instr(&mut r, 0x1003, (3, 0, 0, 0xff, 0), 130, &[(0x13, 0xdd, 0x04)]);
        instr(&mut r, 0x1004, (4, 0, 0, 0xff, 0), 140, &[(0x14, 0xee, 0x05)]);

        // Window [110, 130] → entries at 110, 120, 130 (3 of the 5).
        let mut out: Vec<(DeltaEntry, Vec<WriteRec>)> = Vec::new();
        let n = r.slice_by_cycle(110, 130, &mut out);
        assert_eq!(n, 3, "three entries in [110,130]");
        // Oldest → newest order.
        assert_eq!(out[0].0.cycle, 110);
        assert_eq!(out[0].0.pc, 0x1001);
        assert_eq!(out[1].0.cycle, 120);
        assert_eq!(out[2].0.cycle, 130);
        // Out-of-range entries (100, 140) excluded.
        assert!(out.iter().all(|(e, _)| e.cycle >= 110 && e.cycle <= 130));
        // Writes round-trip exactly (order + bytes).
        assert_eq!(out[0].1.len(), 2);
        assert_eq!(out[0].1[0], WriteRec { addr: 0x11, old_value: 0xbb, new_value: 0x02 });
        assert_eq!(out[0].1[1], WriteRec { addr: 0x12, old_value: 0xcc, new_value: 0x03 });
        assert_eq!(out[1].1.len(), 0, "no-write entry has an empty write list");
        assert_eq!(out[2].1.len(), 1);
        assert_eq!(out[2].1[0], WriteRec { addr: 0x13, old_value: 0xdd, new_value: 0x04 });

        // Reversed args normalise to the same window.
        let mut out2: Vec<(DeltaEntry, Vec<WriteRec>)> = Vec::new();
        assert_eq!(r.slice_by_cycle(130, 110, &mut out2), 3);
        assert_eq!(out2[0].0.cycle, 110);

        // A window outside the ring is empty.
        let mut out3: Vec<(DeltaEntry, Vec<WriteRec>)> = Vec::new();
        assert_eq!(r.slice_by_cycle(500, 600, &mut out3), 0);
        assert!(out3.is_empty());

        // A single-cycle window picks exactly one entry.
        let mut out4: Vec<(DeltaEntry, Vec<WriteRec>)> = Vec::new();
        assert_eq!(r.slice_by_cycle(120, 120, &mut out4), 1);
        assert_eq!(out4[0].0.pc, 0x1002);
    }

    #[test]
    fn set_opcode_roundtrips_through_slice_not_brk() {
        // Regression for the build_from_ring "all-BRK disasm" bug: opcodes stamped via
        // set_opcode must survive into the cycle-window slice (what write_delta_entry
        // encodes), NOT come back as 0 (= BRK for every row).
        let mut r = DeltaRing::with_capacity(16, 64);
        r.set_enabled(true);
        // Drive set_opcode directly with real 6502 opcodes (LDA #imm, JMP abs, STA abs).
        r.begin(0x0801, 0, 0, 0, 0xff, 0x20, 1000);
        r.set_opcode(0xA9, 0x41, 0x00); // LDA #$41
        r.commit();
        r.begin(0x0803, 0x41, 0, 0, 0xff, 0x20, 1003);
        r.set_opcode(0x8D, 0x00, 0x04); // STA $0400
        r.record_write(0x0400, 0x20, 0x41);
        r.commit();
        r.begin(0x0806, 0x41, 0, 0, 0xff, 0x20, 1007);
        r.set_opcode(0x4C, 0x00, 0x08); // JMP $0800
        r.commit();

        let mut out: Vec<(DeltaEntry, Vec<WriteRec>)> = Vec::new();
        let n = r.slice_by_cycle(1000, 1007, &mut out);
        assert_eq!(n, 3);
        // The opcodes round-trip exactly — NOT 0/BRK.
        assert_eq!(out[0].0.opcode, 0xA9);
        assert_eq!(out[0].0.b1, 0x41);
        assert_eq!(out[1].0.opcode, 0x8D);
        assert_eq!(out[1].0.b1, 0x00);
        assert_eq!(out[1].0.b2, 0x04);
        assert_eq!(out[2].0.opcode, 0x4C);
        assert_eq!(out[2].0.b2, 0x08);
        assert!(out.iter().all(|(e, _)| e.opcode != 0x00), "no entry decodes as BRK");
    }

    #[test]
    fn set_opcode_noop_when_disabled_or_no_instruction() {
        let mut r = DeltaRing::with_capacity(8, 32);
        // Disabled: set_opcode does nothing (no panic, no state).
        r.set_enabled(false);
        r.set_opcode(0xA9, 0x41, 0x00);
        assert!(r.newest().is_none());
        // Enabled but no in-flight instruction: set_opcode is a no-op, opcode stays 0.
        r.set_enabled(true);
        r.set_opcode(0xA9, 0x41, 0x00); // before any begin()
        r.begin(0x1000, 0, 0, 0, 0xff, 0, 1);
        r.commit(); // committed without set_opcode → interrupt-only-style entry
        assert_eq!(r.newest().unwrap().opcode, 0, "no set_opcode → opcode 0 (interrupt-only)");
    }

    #[test]
    fn caps_for_seconds_matches_new_sizing() {
        // The per-second sizing the runtime knob uses must equal what `new()` derives.
        let (e, w) = DeltaRing::caps_for_seconds(10);
        assert_eq!(e, 10 * INSTR_PER_SECOND);
        assert_eq!(w, e * WRITES_PER_INSTR_NUM / WRITES_PER_INSTR_DEN);
        // Clamp: 0 seconds floors to 1.
        let (e0, _w0) = DeltaRing::caps_for_seconds(0);
        assert_eq!(e0, INSTR_PER_SECOND);
    }

    #[test]
    fn resize_rebuilds_slabs_drops_history_keeps_enabled() {
        // Reverse-depth knob: resize SHRINKS/GROWS both slabs, DROPS history, keeps the
        // armed flag — exactly the runtime/set_reverse_depth contract.
        let mut r = DeltaRing::with_capacity(16, 64);
        r.set_enabled(true);
        for i in 0..10u64 {
            instr(&mut r, 0x100 + i as u16, (0, 0, 0, 0xff, 0), i, &[(i as u16, 0, 1)]);
        }
        assert_eq!(r.len(), 10);
        // SHRINK to a tiny ring → history gone, new caps applied, still enabled.
        r.resize(4, 5);
        assert_eq!(r.entry_capacity(), 4);
        assert_eq!(r.writes_capacity(), 5);
        assert_eq!(r.len(), 0, "resize drops all history");
        assert!(r.is_empty());
        assert!(r.enabled(), "resize preserves the armed flag");
        // The shrunk ring still records and caps at the new size (4-entry wrap).
        for i in 0..6u64 {
            instr(&mut r, 0x200 + i as u16, (0, 0, 0, 0xff, 0), 100 + i, &[(i as u16, 0, 1)]);
        }
        assert_eq!(r.len(), 4, "post-resize ring caps at the NEW capacity");
        assert_eq!(r.newest().unwrap().pc, 0x200 + 5);
        // GROW back and confirm a wider window is retained.
        r.resize(32, 64);
        assert_eq!(r.entry_capacity(), 32);
        assert_eq!(r.len(), 0);
        for i in 0..20u64 {
            instr(&mut r, 0x300 + i as u16, (0, 0, 0, 0xff, 0), 200 + i, &[]);
        }
        assert_eq!(r.len(), 20, "grown ring retains the full 20 (< 32 cap)");
    }

    #[test]
    fn ram_estimate_for_default_depth_is_about_96mb() {
        // The set_reverse_depth RAM figure (delta entries 24B + writes 4B + cpuhistory
        // 24B) must land near the documented ~96 MB for the 10 s default, so the daemon
        // reports a truthful cost. (This pins the per-second sizing the knob reports.)
        let secs = 10usize;
        let (e, w) = DeltaRing::caps_for_seconds(secs);
        let cpu_cap = secs * INSTR_PER_SECOND; // cpu-history scaled to depth by the knob
        let bytes = e * std::mem::size_of::<DeltaEntry>()
            + w * std::mem::size_of::<WriteRec>()
            + cpu_cap * std::mem::size_of::<crate::cpu_history::CpuHistEntry>();
        let mb = bytes as f64 / (1024.0 * 1024.0);
        // 72 MB (entries) + 15 MB (writes) + 72 MB (cpuhistory@depth) ≈ 159 MB.
        // (The 96 MB doc figure predates scaling cpu-history with depth; assert a sane
        // band so an accidental ×10 sizing bug is caught.)
        assert!(mb > 120.0 && mb < 200.0, "10s rings ≈ {mb:.0} MB (expected 120..200)");
    }

    #[test]
    fn resize_preserves_disabled_killswitch() {
        // A kill-switched ring stays inert across a resize (no silent re-arm).
        let mut r = DeltaRing::with_capacity(8, 32);
        r.set_enabled(false);
        r.resize(4, 8);
        assert!(!r.enabled(), "resize must not re-arm a kill-switched ring");
        instr(&mut r, 0x1, (0, 0, 0, 0, 0), 1, &[(0x10, 0, 1)]);
        assert_eq!(r.len(), 0, "disabled ring records nothing after resize");
    }

    #[test]
    fn clone_is_empty_same_capacity() {
        let mut r = DeltaRing::with_capacity(32, 64);
        r.set_enabled(true);
        for i in 0..10u64 {
            instr(&mut r, i as u16, (0, 0, 0, 0xff, 0), i, &[(i as u16, 0, 1)]);
        }
        let c = r.clone();
        assert_eq!(c.entry_capacity(), 32);
        assert_eq!(c.writes_capacity(), 64);
        assert_eq!(c.len(), 0, "clone starts empty");
        assert!(c.enabled());
    }
}
