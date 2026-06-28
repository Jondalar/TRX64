//! crash_triage.rs — reverse-debug Phase 2: guided crash-triage on a JAMmed machine.
//!
//! When the C64 CPU JAMs (a KIL/illegal $x2 opcode, or execution derailed into a wild
//! PC that decodes to a JAM), the user wants the CAUSAL CHAIN auto-printed instead of
//! hand-walking it. The classic derail is an `RTS` that pops a CORRUPTED return address
//! off the stack and jumps into the wild. This module reads the two always-on rings and
//! reconstructs that chain:
//!
//!   1. **Crash point** — the JAM PC + its (illegal) opcode + the few instructions that
//!      led into it (from the CPU-history ring, which carries opcodes).
//!   2. **The wild control transfer** — walk the CPU-history ring BACKWARD to the most
//!      recent control-flow instruction that LANDED at the wild PC: an `RTS`/`RTI` (pops
//!      a return address off the stack), an indirect `JMP ($6C)`, or a `JMP`/`JSR` to an
//!      out-of-range target.
//!   3. **The corruptor** (only when the transfer was a STACK POP — `RTS`/`RTI`) — the
//!      stack slot(s) the pop read (`$0100+SP` at that moment), then `who_wrote` those
//!      slots → the instruction that wrote the bad byte (PC, cycle, old→new).
//!
//! HONESTY CONTRACT: this is a PRAGMATIC heuristic, not a proof. Every step carries a
//! `confidence` and a `note`. When the wild transfer is NOT a stack pop (a wild indirect
//! JMP, or a direct JMP/JSR to a computed/corrupt target), the module reports what it CAN
//! (the transfer + who wrote the vector/target bytes when reachable) and marks the
//! stack-corruptor step ABSENT — it never fabricates a stack slot. When the history ring
//! does not cover the run-up (cleared by a reset, or the transfer fell off the back), the
//! chain says so rather than inventing a transfer.
//!
//! READ-ONLY: triage never mutates the machine. It is safe to run on the live JAMmed
//! state (the JAM drop-in) and to re-run on demand (`triage` verb / `runtime/crash_triage`).

use crate::cpu_history::CpuHistEntry;
use crate::delta_ring::DeltaRing;

/// 6502 control-flow opcodes the triage classifies.
const OP_JSR: u8 = 0x20;
const OP_RTI: u8 = 0x40;
const OP_JMP_ABS: u8 = 0x4c;
const OP_RTS: u8 = 0x60;
const OP_JMP_IND: u8 = 0x6c;

/// How sure the triage is about a given step. Surfaced so the user (and the JSON
/// consumer) can tell a pinned fact from a best-effort guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    /// Directly read from a ring (an opcode in the CPU-history ring, a write in the
    /// delta ring) — a recorded fact, not an inference.
    Certain,
    /// A reasonable inference from the rings (e.g. "this RTS landed at the JAM PC
    /// because it is the most recent transfer before it") that the rings strongly
    /// support but cannot prove byte-for-byte.
    Likely,
    /// A weak inference, or a value reconstructed where the ring coverage is partial.
    /// Present so the user knows to double-check it.
    Low,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Certain => "certain",
            Confidence::Likely => "likely",
            Confidence::Low => "low",
        }
    }
}

/// The kind of wild control transfer the triage identified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferKind {
    /// `RTS` — pops a 2-byte return address (the classic stack-smash derail).
    Rts,
    /// `RTI` — pops status + a 2-byte return address.
    Rti,
    /// `JMP ($6C)` — jumps through a 16-bit vector in memory.
    JmpIndirect,
    /// `JMP $4C` — direct absolute jump (target is the operand).
    JmpAbs,
    /// `JSR $20` — direct call (target is the operand).
    Jsr,
    /// No control-flow instruction was found before the wild PC in the ring window —
    /// the derail predates the ring coverage, or PC fell off the end of a block.
    Unknown,
}

impl TransferKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TransferKind::Rts => "RTS",
            TransferKind::Rti => "RTI",
            TransferKind::JmpIndirect => "JMP(ind)",
            TransferKind::JmpAbs => "JMP",
            TransferKind::Jsr => "JSR",
            TransferKind::Unknown => "?",
        }
    }
    /// Whether this transfer popped a return address off the stack (so the
    /// stack-corruptor analysis applies).
    pub fn is_stack_pop(self) -> bool {
        matches!(self, TransferKind::Rts | TransferKind::Rti)
    }
}

/// The crash point — where the machine JAMmed.
#[derive(Clone, Debug)]
pub struct CrashPoint {
    /// The PC the CPU jammed at (the wild PC).
    pub pc: u16,
    /// The opcode byte at that PC (the illegal/KIL opcode that jammed).
    pub opcode: u8,
    /// The handful of instructions that executed leading into the crash (oldest →
    /// newest), from the CPU-history ring — distinct PCs, with the repeated JAM-PC
    /// re-fetches collapsed to one. Empty if the ring is disabled/empty.
    pub lead_in: Vec<CpuHistEntry>,
}

/// The wild control transfer that landed at the crash PC.
#[derive(Clone, Debug)]
pub struct WildTransfer {
    pub kind: TransferKind,
    /// The address of the transfer instruction itself.
    pub at_pc: u16,
    /// Where it landed (= the crash PC, for a transfer we accepted).
    pub landed_pc: u16,
    /// For a STACK POP: the SP value BEFORE the pop (so the popped slots are
    /// `$0100+((sp+1)&0xff)` / `+2`). `None` for a non-pop transfer.
    pub pre_sp: Option<u8>,
    /// For `JMP ($6C)`: the vector address the operand pointed at (where the bad
    /// target bytes were read from). `None` otherwise.
    pub vector_addr: Option<u16>,
    pub confidence: Confidence,
    pub note: String,
    /// TRX64 feature-request #3 — true when the wild transfer could NOT be identified
    /// because it lies OLDER than the live history ring (the ring window is too short),
    /// as opposed to a transfer that was genuinely indeterminate from a present window.
    /// Drives the typed `ring_exhausted` signal so the user reads "bump revdepth /
    /// trigger earlier", not "no data / wrong PC".
    pub ring_bound: bool,
}

/// The stack slot a pop read, and who last wrote it (the corruptor candidate).
#[derive(Clone, Debug)]
pub struct StackSlot {
    /// The stack address read by the pop (`$0100..$01FF`).
    pub addr: u16,
    /// The byte currently at that slot (the popped byte — read from RAM, the wild
    /// return-address byte).
    pub value: u8,
    /// The last writer of this slot found in the delta ring, if any.
    pub writer_pc: Option<u16>,
    pub writer_cycle: Option<u64>,
    pub writer_old: Option<u8>,
    pub writer_new: Option<u8>,
    pub confidence: Confidence,
    pub note: String,
}

/// The full triage chain.
#[derive(Clone, Debug)]
pub struct TriageChain {
    pub crash: CrashPoint,
    /// The wild transfer (always present; `kind == Unknown` when none was found).
    pub transfer: WildTransfer,
    /// The stack slots the pop read + their last writers. Empty for a non-pop transfer
    /// (the triage refuses to invent a stack corruptor).
    pub corruptor_slots: Vec<StackSlot>,
    /// True when the crash was a genuine stack-smash and the triage pinned a writer for
    /// at least one popped slot (the high-value case).
    pub pinned_corruptor: bool,
    /// TRX64 feature-request #1 — the PINNED loop/halt onset: the control transfer that
    /// ENTERED the tight loop / halt-trap the machine is now spinning in, captured at
    /// onset and exempt from ring eviction. `Some` when the always-on detector pinned a
    /// loop entry since the last timeline boundary (it survives even after the
    /// spinning-storm evicted the transfer from the live history ring — the recurring
    /// "transfer older than the ring" blind spot). `None` when no loop was detected.
    pub loop_onset: Option<crate::delta_ring::LoopOnset>,
    /// A human-readable one-line summary (the compact chain) + any honesty caveats.
    pub summary: String,
}

/// Inputs the triage needs from the machine, decoupled from `Machine` so the function is
/// unit-testable with synthetic rings + a memory closure.
pub struct TriageInputs<'a, R: Fn(u16) -> u8> {
    /// The CPU-history ring entries, OLDEST → NEWEST (= `CpuHistoryRing::last_n`).
    pub history: &'a [CpuHistEntry],
    /// The full-delta ring (for `who_wrote` of a stack slot).
    pub delta: &'a DeltaRing,
    /// The live (crashed) PC — where the CPU jammed.
    pub crash_pc: u16,
    /// Side-effect-free memory read (banked, IO-aware): used to read the crash opcode
    /// and the popped stack bytes. (`Machine::read_full`.)
    pub read: R,
}

/// reverse-debug Phase 2 — build the causal chain from the two always-on rings.
///
/// Walks the CPU-history ring back from the JAM PC to the wild transfer, then (for a
/// stack pop) reads the popped slots and `who_wrote`s them. Pragmatic + HONEST: each
/// step is confidence-tagged and the non-pop case is reported without fabricating a
/// stack corruptor.
pub fn triage<R: Fn(u16) -> u8>(inp: TriageInputs<R>) -> TriageChain {
    let crash_pc = inp.crash_pc;
    let crash_opcode = (inp.read)(crash_pc);

    // ── 1. Crash lead-in: the last few DISTINCT instructions before/at the crash.
    // A JAMmed CPU re-fetches the same JAM opcode every cycle, so the ring's tail is
    // many identical (crash_pc) entries — collapse consecutive duplicates and keep the
    // last ~6 distinct PCs for context.
    let lead_in = collapse_tail(inp.history, 6);

    // ── 2. The wild transfer. Find the newest history entry that LANDED at the crash
    // PC (its PC == crash_pc), then the instruction that EXECUTED IMMEDIATELY BEFORE it
    // is the transfer that produced that landing.
    let transfer = find_wild_transfer(inp.history, crash_pc);

    // ── 3. The corruptor — only for a STACK POP (RTS/RTI). Read the popped slot(s) and
    // who_wrote them. Never fabricate for a non-pop transfer.
    let mut corruptor_slots = Vec::new();
    let mut pinned_corruptor = false;
    if let Some(pre_sp) = transfer.pre_sp {
        // RTS pops 2 bytes (lo @ sp+1, hi @ sp+2). RTI pops P first (sp+1) then the
        // 2-byte return address (sp+2 lo, sp+3 hi). We focus on the RETURN-ADDRESS
        // bytes — those are what derailed the PC.
        let (lo_off, hi_off) = match transfer.kind {
            TransferKind::Rti => (2u8, 3u8),
            _ => (1u8, 2u8), // RTS
        };
        for (label, off) in [("ret-lo", lo_off), ("ret-hi", hi_off)] {
            let addr = 0x0100u16 | (pre_sp.wrapping_add(off) as u16);
            let value = (inp.read)(addr);
            // who_wrote the slot (newest writer first; take 1).
            let hits = inp.delta.who_wrote(addr, 1);
            let mut slot = StackSlot {
                addr,
                value,
                writer_pc: None,
                writer_cycle: None,
                writer_old: None,
                writer_new: None,
                confidence: Confidence::Likely,
                note: format!("{label} of the popped return address"),
            };
            if let Some((e, w)) = hits.first() {
                slot.writer_pc = Some(e.pc);
                slot.writer_cycle = Some(e.cycle);
                slot.writer_old = Some(w.old_value);
                slot.writer_new = Some(w.new_value);
                slot.confidence = Confidence::Certain;
                slot.note = format!("{label}: last written by ${:04X}", e.pc);
                pinned_corruptor = true;
            } else {
                slot.confidence = Confidence::Low;
                slot.note = format!(
                    "{label}: no writer in the live ring (the corruptor predates the \
                     reverse window, or this byte is the original stacked value)"
                );
            }
            corruptor_slots.push(slot);
        }
    }

    // ── 4. TRX64 feature-request #1 — the PINNED loop/halt onset. The always-on
    // detector pinned the entry transfer the instant the tight loop / halt began, so it
    // is available even when the spinning-storm has long since evicted that transfer
    // from the live history ring. Read-only.
    let loop_onset = inp.delta.loop_onset();

    let crash = CrashPoint { pc: crash_pc, opcode: crash_opcode, lead_in };
    let summary =
        format_summary(&crash, &transfer, &corruptor_slots, pinned_corruptor, loop_onset.as_ref());
    TriageChain { crash, transfer, corruptor_slots, pinned_corruptor, loop_onset, summary }
}

/// Collapse the ring tail's consecutive duplicate PCs (the JAM re-fetch storm) and keep
/// the last `keep` distinct-PC entries, OLDEST → NEWEST.
fn collapse_tail(history: &[CpuHistEntry], keep: usize) -> Vec<CpuHistEntry> {
    let mut distinct: Vec<CpuHistEntry> = Vec::new();
    let mut last_pc: Option<u16> = None;
    for e in history {
        if Some(e.pc) != last_pc {
            distinct.push(*e);
            last_pc = Some(e.pc);
        }
    }
    let n = distinct.len();
    if n > keep {
        distinct.split_off(n - keep)
    } else {
        distinct
    }
}

/// Walk the CPU-history ring to the transfer that landed at `crash_pc`.
///
/// Strategy: scan newest → oldest for the FIRST entry whose `pc == crash_pc` (the first
/// time the CPU executed AT the wild PC — earlier identical re-fetches are the JAM
/// storm). The entry IMMEDIATELY BEFORE that one (in chronological order) is the
/// instruction that transferred control to the crash PC. Classify its opcode.
fn find_wild_transfer(history: &[CpuHistEntry], crash_pc: u16) -> WildTransfer {
    // Default: nothing found.
    let mut out = WildTransfer {
        kind: TransferKind::Unknown,
        at_pc: 0,
        landed_pc: crash_pc,
        pre_sp: None,
        vector_addr: None,
        confidence: Confidence::Low,
        note: "no control-flow instruction found before the crash PC in the live \
               history ring (the derail predates the ring, or PC walked off a block)"
            .to_string(),
        ring_bound: false,
    };
    if history.is_empty() {
        out.note =
            "CPU-history ring is empty (history disabled, or cleared by a recent reset)"
                .to_string();
        return out;
    }

    // Find the OLDEST index of the contiguous run of crash_pc entries at the tail — i.e.
    // the first time the CPU landed at crash_pc. Walk newest→oldest while pc==crash_pc;
    // the boundary's predecessor is the transfer. If crash_pc never appears (the crash
    // PC was reached but not yet retired into the ring), fall back to the newest entry's
    // successor logic by treating the last entry as the landing.
    let mut landing_idx: Option<usize> = None;
    // First locate the newest entry equal to crash_pc.
    for i in (0..history.len()).rev() {
        if history[i].pc == crash_pc {
            // Now walk further back while still crash_pc to find the FIRST landing.
            let mut j = i;
            while j > 0 && history[j - 1].pc == crash_pc {
                j -= 1;
            }
            landing_idx = Some(j);
            break;
        }
    }

    // If we never saw crash_pc in the ring, the JAM instruction itself was not retired
    // into the ring (e.g. a wild PC that decoded to JAM on the very first fetch). Treat
    // the NEWEST retired instruction as the transfer candidate (the last thing that ran
    // before the wild fetch).
    let transfer_idx = match landing_idx {
        Some(0) => {
            // The landing is the oldest entry in the window — the transfer that produced
            // it fell off the back of the ring. TRX64 feature-request #3: this is the
            // ring-boundary case (window too short), not an indeterminate one.
            out.confidence = Confidence::Low;
            out.ring_bound = true;
            out.note = format!(
                "the crash PC ${crash_pc:04X} is the oldest instruction still in the \
                 ring — the transfer that jumped here is older than the reverse window"
            );
            return out;
        }
        Some(j) => j - 1,
        None => {
            // crash_pc not retired: the newest entry is the predecessor that ran before
            // the wild fetch.
            history.len() - 1
        }
    };

    let t = history[transfer_idx];
    out.at_pc = t.pc;
    // The SP value BEFORE a pop = the SP AFTER the instruction that ran before the
    // transfer (post-regs of `transfer_idx - 1`), because the history stores POST regs.
    // For RTS/RTI the popped slots are computed from this pre-pop SP.
    let pre_sp = if transfer_idx >= 1 {
        Some(history[transfer_idx - 1].sp)
    } else {
        // No predecessor in the window — fall back to the transfer entry's own post-SP
        // adjusted: an RTS leaves SP = pre_sp + 2, so pre_sp = post_sp - 2. Best-effort.
        Some(t.sp.wrapping_sub(2))
    };

    match t.opcode {
        OP_RTS => {
            out.kind = TransferKind::Rts;
            out.pre_sp = pre_sp;
            out.confidence = if transfer_idx >= 1 { Confidence::Certain } else { Confidence::Likely };
            out.note = "RTS popped a 2-byte return address off the stack".to_string();
        }
        OP_RTI => {
            out.kind = TransferKind::Rti;
            out.pre_sp = pre_sp;
            out.confidence = if transfer_idx >= 1 { Confidence::Certain } else { Confidence::Likely };
            out.note = "RTI popped status + a 2-byte return address off the stack".to_string();
        }
        OP_JMP_IND => {
            out.kind = TransferKind::JmpIndirect;
            // The operand (b1/b2) is the vector address the JMP read its target from.
            out.vector_addr = Some(u16::from_le_bytes([t.b1, t.b2]));
            out.confidence = Confidence::Certain;
            out.note = format!(
                "JMP (${:04X}) jumped through a memory vector — not a stack pop; the \
                 corruptor is whoever wrote the vector bytes (indeterminate as a stack \
                 smash)",
                out.vector_addr.unwrap()
            );
        }
        OP_JMP_ABS => {
            out.kind = TransferKind::JmpAbs;
            out.confidence = Confidence::Certain;
            out.note =
                "direct JMP to the crash PC — the target is the literal operand, not a \
                 stack value (if this is wrong, the operand bytes themselves were \
                 self-modified)"
                    .to_string();
        }
        OP_JSR => {
            out.kind = TransferKind::Jsr;
            out.confidence = Confidence::Certain;
            out.note =
                "direct JSR to the crash PC — the target is the literal operand, not a \
                 stack value"
                    .to_string();
        }
        other => {
            // The instruction before the landing was NOT a control transfer. This means
            // PC fell THROUGH into the wild (the previous block ran off its end into bad
            // bytes), OR the real transfer is older. Report it honestly.
            out.kind = TransferKind::Unknown;
            out.at_pc = t.pc;
            out.confidence = Confidence::Low;
            out.note = format!(
                "the instruction before the crash PC (${:04X}, op ${other:02X}) is not a \
                 control transfer — execution likely fell through into the wild rather \
                 than jumping; no stack pop to attribute",
                t.pc
            );
        }
    }
    out
}

/// Build the compact one-line chain + honesty caveats.
fn format_summary(
    crash: &CrashPoint,
    transfer: &WildTransfer,
    slots: &[StackSlot],
    pinned: bool,
    loop_onset: Option<&crate::delta_ring::LoopOnset>,
) -> String {
    // TRX64 feature-request #1 — surface the PINNED loop entry first when present: the
    // transfer that ENTERED the spin is the root-cause fact the ring otherwise loses.
    let loop_prefix = match loop_onset {
        Some(lo) => format!(
            "loop entry: ${:04X} -> ${:04X} @cyc {}  |  ",
            lo.src_pc, lo.dst_pc, lo.cycle
        ),
        None => String::new(),
    };
    let body = format_summary_body(crash, transfer, slots, pinned);
    format!("{loop_prefix}{body}")
}

/// The crash/transfer/corruptor part of the summary (without the loop-onset prefix).
fn format_summary_body(
    crash: &CrashPoint,
    transfer: &WildTransfer,
    slots: &[StackSlot],
    pinned: bool,
) -> String {
    let head = format!("JAM @ ${:04X} (op ${:02X})", crash.pc, crash.opcode);
    match transfer.kind {
        TransferKind::Rts | TransferKind::Rti => {
            // Pin the popped return address from the slot bytes (lo, hi).
            let lo = slots.first().map(|s| s.value).unwrap_or(0);
            let hi = slots.get(1).map(|s| s.value).unwrap_or(0);
            let wild = u16::from_le_bytes([lo, hi]);
            let slot_lo = slots.first().map(|s| s.addr).unwrap_or(0);
            let mut s = format!(
                "{head} ← wild {} @ ${:04X} popped ${:04X} from ${:04X}",
                transfer.kind.as_str(),
                transfer.at_pc,
                wild,
                slot_lo
            );
            if pinned {
                // Cite the writer(s) of the slot(s).
                for slot in slots {
                    if let (Some(wp), Some(wc), Some(old), Some(new)) =
                        (slot.writer_pc, slot.writer_cycle, slot.writer_old, slot.writer_new)
                    {
                        s.push_str(&format!(
                            " ← ${:04X} written by ${:04X} @ cyc {} (${:02X}→${:02X})",
                            slot.addr, wp, wc, old, new
                        ));
                    }
                }
            } else {
                s.push_str(
                    "  [corruptor NOT pinned: no writer for the popped slots in the live \
                     ring — the bad byte was stacked before the reverse window, or is the \
                     genuine (non-smashed) return]",
                );
            }
            s
        }
        TransferKind::JmpIndirect => format!(
            "{head} ← wild JMP(ind) @ ${:04X} through vector ${:04X} → ${:04X}  \
             [NOT a stack smash — corruptor = whoever wrote the vector bytes; \
             indeterminate from the stack]",
            transfer.at_pc,
            transfer.vector_addr.unwrap_or(0),
            crash.pc
        ),
        TransferKind::JmpAbs | TransferKind::Jsr => format!(
            "{head} ← {} @ ${:04X} → ${:04X}  [direct transfer to the wild target; not a \
             stack smash — if unexpected, the operand bytes were self-modified]",
            transfer.kind.as_str(),
            transfer.at_pc,
            crash.pc
        ),
        TransferKind::Unknown => format!(
            "{head} ← {}  [the wild control transfer is indeterminate from the live ring]",
            transfer.note
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta_ring::DeltaRing;

    fn hist(pc: u16, opcode: u8, b1: u8, b2: u8, sp: u8, cycle: u64) -> CpuHistEntry {
        CpuHistEntry { cycle, pc, opcode, b1, b2, a: 0, x: 0, y: 0, sp, p: 0 }
    }

    /// A memory closure backed by a flat 64K array.
    fn mem(ram: &[u8; 0x10000]) -> impl Fn(u16) -> u8 + '_ {
        move |a: u16| ram[a as usize]
    }

    #[test]
    fn classifies_rts_stack_smash_and_pins_corruptor() {
        // Scenario: a routine wrote a BAD return address ($DEAD-1 = $DEAC, so RTS lands
        // at $DEAD) onto the stack via two STAs, then RTS popped it → JAM at $DEAD.
        let mut ram = [0u8; 0x10000];
        // SP before RTS = $FB → popped slots $01FC (lo) / $01FD (hi). RTS lands at
        // stacked_addr + 1, so to land at $DEAD we stack $DEAC.
        ram[0x01fc] = 0xac; // ret-lo
        ram[0x01fd] = 0xde; // ret-hi
        ram[0xdead] = 0x02; // the JAM opcode at the wild PC.

        // Delta ring: the smashing instruction @ $1234 wrote $01FC, and @ $1237 wrote
        // $01FD (the corruptor).
        let mut dr = DeltaRing::with_capacity(64, 256);
        dr.set_enabled(true);
        dr.begin(0x1234, 0, 0, 0, 0xff, 0, 100);
        dr.record_write(0x01fc, 0x00, 0xac);
        dr.commit();
        dr.begin(0x1237, 0, 0, 0, 0xff, 0, 102);
        dr.record_write(0x01fd, 0x00, 0xde);
        dr.commit();

        // CPU history (oldest → newest): a predecessor (sets pre-pop SP=$FB), then the
        // RTS @ $E5A0, then the JAM re-fetch storm at $DEAD.
        let history = vec![
            hist(0xe59f, 0xea, 0, 0, 0xfb, 200), // predecessor: post-SP = $FB = pre-pop SP
            hist(0xe5a0, OP_RTS, 0, 0, 0xfd, 201), // RTS: post-SP = $FD (popped 2)
            hist(0xdead, 0x02, 0, 0, 0xfd, 202),   // landed (JAM)
            hist(0xdead, 0x02, 0, 0, 0xfd, 203),   // re-fetch storm
            hist(0xdead, 0x02, 0, 0, 0xfd, 204),
        ];

        let chain = triage(TriageInputs {
            history: &history,
            delta: &dr,
            crash_pc: 0xdead,
            read: mem(&ram),
        });

        // (i) names the JAM/wild PC + opcode.
        assert_eq!(chain.crash.pc, 0xdead);
        assert_eq!(chain.crash.opcode, 0x02);
        // (ii) identifies the RTS stack pop @ $E5A0.
        assert_eq!(chain.transfer.kind, TransferKind::Rts);
        assert_eq!(chain.transfer.at_pc, 0xe5a0);
        assert_eq!(chain.transfer.pre_sp, Some(0xfb));
        // (iii) who_wrote pins the smashing instruction(s).
        assert!(chain.pinned_corruptor);
        assert_eq!(chain.corruptor_slots.len(), 2);
        assert_eq!(chain.corruptor_slots[0].addr, 0x01fc);
        assert_eq!(chain.corruptor_slots[0].writer_pc, Some(0x1234));
        assert_eq!(chain.corruptor_slots[1].addr, 0x01fd);
        assert_eq!(chain.corruptor_slots[1].writer_pc, Some(0x1237));
        // Summary names the popped wild address.
        assert!(chain.summary.contains("$DEAD"));
        assert!(chain.summary.contains("RTS"));
        assert!(chain.summary.contains("$1234") || chain.summary.contains("$1237"));
    }

    #[test]
    fn indirect_jmp_reports_vector_does_not_fabricate_stack() {
        // A wild JMP ($00FE) where the vector held a bad target → JAM. The triage must
        // report the vector and NOT invent a stack corruptor.
        let mut ram = [0u8; 0x10000];
        ram[0x4000] = 0x02; // JAM at the wild target.
        let dr = DeltaRing::with_capacity(8, 32);
        let history = vec![
            hist(0x2000, 0xea, 0, 0, 0xff, 10),
            hist(0x2001, OP_JMP_IND, 0xfe, 0x00, 0xff, 11), // JMP ($00FE)
            hist(0x4000, 0x02, 0, 0, 0xff, 12),
        ];
        let chain = triage(TriageInputs {
            history: &history,
            delta: &dr,
            crash_pc: 0x4000,
            read: mem(&ram),
        });
        assert_eq!(chain.transfer.kind, TransferKind::JmpIndirect);
        assert_eq!(chain.transfer.vector_addr, Some(0x00fe));
        // No stack corruptor fabricated.
        assert!(chain.corruptor_slots.is_empty());
        assert!(!chain.pinned_corruptor);
        assert!(chain.summary.contains("vector"));
    }

    #[test]
    fn rts_with_no_writer_is_honest_low_confidence() {
        // RTS into the wild, but the corrupted byte was never written in the ring window
        // → the triage must NOT pin a corruptor and must say so.
        let mut ram = [0u8; 0x10000];
        ram[0x01fc] = 0x33;
        ram[0x01fd] = 0xc0; // → lands $C034
        ram[0xc034] = 0x02;
        let dr = DeltaRing::with_capacity(8, 32); // empty ring: no writers.
        let history = vec![
            hist(0xb000, 0xea, 0, 0, 0xfb, 50),
            hist(0xb001, OP_RTS, 0, 0, 0xfd, 51),
            hist(0xc034, 0x02, 0, 0, 0xfd, 52),
        ];
        let chain = triage(TriageInputs {
            history: &history,
            delta: &dr,
            crash_pc: 0xc034,
            read: mem(&ram),
        });
        assert_eq!(chain.transfer.kind, TransferKind::Rts);
        assert!(!chain.pinned_corruptor);
        // Slots exist (we read them) but have no writer + are low-confidence.
        assert_eq!(chain.corruptor_slots.len(), 2);
        assert!(chain.corruptor_slots.iter().all(|s| s.writer_pc.is_none()));
        assert!(chain.corruptor_slots.iter().all(|s| s.confidence == Confidence::Low));
        assert!(chain.summary.contains("NOT pinned"));
    }

    #[test]
    fn fall_through_into_wild_reports_unknown_transfer() {
        // The instruction before the crash PC is NOT a control transfer (execution fell
        // through into bad bytes). The triage must report Unknown, not guess a pop.
        let mut ram = [0u8; 0x10000];
        ram[0x3001] = 0x02;
        let dr = DeltaRing::with_capacity(8, 32);
        let history = vec![
            hist(0x2fff, 0xea, 0, 0, 0xff, 5), // NOP
            hist(0x3000, 0xea, 0, 0, 0xff, 6), // NOP → falls through to $3001
            hist(0x3001, 0x02, 0, 0, 0xff, 7), // JAM
        ];
        let chain = triage(TriageInputs {
            history: &history,
            delta: &dr,
            crash_pc: 0x3001,
            read: mem(&ram),
        });
        assert_eq!(chain.transfer.kind, TransferKind::Unknown);
        assert!(chain.corruptor_slots.is_empty());
        assert!(!chain.pinned_corruptor);
    }

    #[test]
    fn loop_onset_surfaces_in_triage_chain_after_spin_storm() {
        // FEATURE #1: a halt-trap `JMP $self`. Even when the live history ring is the
        // generic JAM/wild path, the PINNED loop onset must appear in the chain +
        // summary so `triage` reports "loop entry: $SRC → $DST" after seconds spinning.
        let ram = [0u8; 0x10000];
        // A tiny delta ring that the spin-storm wraps completely.
        let mut dr = DeltaRing::with_capacity(4, 8);
        dr.set_enabled(true);
        // The entry transfer: JSR @ $07A6 → $0900.
        dr.begin(0x07a6, 0, 0, 0, 0xfb, 0, 4242);
        dr.set_opcode(0x20, 0x00, 0x09);
        dr.commit();
        // Spin `JMP $0900` thousands of times (wraps the 4-entry ring many times over).
        for i in 0..3000u64 {
            dr.begin(0x0900, 0, 0, 0, 0xfb, 0, 4243 + i);
            dr.set_opcode(0x4c, 0x00, 0x09);
            dr.commit();
        }
        // The history ring fed to triage holds only the spin tail (the JSR is evicted).
        let history = vec![
            hist(0x0900, 0x4c, 0x00, 0x09, 0xfb, 7000),
            hist(0x0900, 0x4c, 0x00, 0x09, 0xfb, 7001),
        ];
        let chain = triage(TriageInputs {
            history: &history,
            delta: &dr,
            crash_pc: 0x0900,
            read: mem(&ram),
        });
        // The pinned loop onset is present + names the entry transfer the ring lost.
        let lo = chain.loop_onset.expect("loop onset in chain");
        assert_eq!(lo.src_pc, 0x07a6);
        assert_eq!(lo.dst_pc, 0x0900);
        assert_eq!(lo.cycle, 4242);
        assert!(chain.summary.contains("loop entry"), "summary surfaces the loop entry");
        assert!(chain.summary.contains("$07A6"));
    }

    #[test]
    fn no_loop_onset_leaves_chain_field_none() {
        // FEATURE #1: when no loop was detected the field is None (a normal stack-smash
        // triage is unaffected — no spurious loop line).
        let mut ram = [0u8; 0x10000];
        ram[0x01fc] = 0xac;
        ram[0x01fd] = 0xde;
        ram[0xdead] = 0x02;
        let dr = DeltaRing::with_capacity(8, 32); // empty ring → no onset.
        let history = vec![
            hist(0xe59f, 0xea, 0, 0, 0xfb, 200),
            hist(0xe5a0, OP_RTS, 0, 0, 0xfd, 201),
            hist(0xdead, 0x02, 0, 0, 0xfd, 202),
        ];
        let chain = triage(TriageInputs {
            history: &history,
            delta: &dr,
            crash_pc: 0xdead,
            read: mem(&ram),
        });
        assert!(chain.loop_onset.is_none());
        assert!(!chain.summary.contains("loop entry"));
    }

    #[test]
    fn empty_history_is_honest() {
        let ram = [0u8; 0x10000];
        let dr = DeltaRing::with_capacity(8, 32);
        let chain = triage(TriageInputs {
            history: &[],
            delta: &dr,
            crash_pc: 0x1111,
            read: mem(&ram),
        });
        assert_eq!(chain.transfer.kind, TransferKind::Unknown);
        assert!(chain.transfer.note.contains("empty"));
    }
}
