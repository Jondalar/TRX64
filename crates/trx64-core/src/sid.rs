//! sid.rs — SID 6581 osc/envelope model (B-level, no audio PCM).
//!
//! Ported 1:1 from the TS oracle's `headless/sid/sid.ts` (Spec 151).
//! Implements the oscillator phase-advance + ADSR state machine needed for
//! the two computed-read registers:
//!
//!   $D41B (OSC3) — voice-3 oscillator output MSB (waveform-dependent).
//!   $D41C (ENV3) — voice-3 envelope generator value (0..255).
//!
//! All other SID registers are write-only on real hardware. They are stored
//! in `Machine::sid_regs` (the existing 32-byte shadow), which this struct
//! references on reads.
//!
//! The SID trace domain (op 0x22 SID_REG_WRITE) is RESERVED: the TS oracle
//! has no live producer for it (confirmed empirically — verified same way as
//! the VIC domain in ADR-015). SID writes reach the trace only as op-0x11
//! RAM_WRITE through the regular CPU bus tap. No SID trace frames are ever
//! emitted by this implementation.
//!
//! PCM audio output (reSID sample generation, WAV export) is Phase-1.5 —
//! OUT OF SCOPE here. No sample buffers, no audio callbacks.
//!
//! Pure / sync / deterministic — no async, no rand, no time. Clone-able with
//! the Machine for Phase-2 COW forks.

// ── ADSR state codes (VICE fastsid.c lines 65-69) ────────────────────────────

pub const ADSR_ATTACK: u8  = 0;
pub const ADSR_DECAY: u8   = 1;
pub const ADSR_SUSTAIN: u8 = 2;
pub const ADSR_RELEASE: u8 = 3;
pub const ADSR_IDLE: u8    = 4;

// ── ADSR rate table (PAL 985248 Hz; Yannes datasheet / VICE adrtable scaled) ──
//
// Attack: cycles per envelope step (±1 out of 255) for each 4-bit code.
// Decay/release = attack × 3 (VICE exptable scale 1).

const ATTACK_CYCLES: [u32; 16] = [
    9, 32, 63, 95, 149, 220, 267, 313,
    392, 977, 1954, 3126, 3907, 11720, 19532, 31251,
];

/// Decay / release cycles per envelope step.
#[inline]
fn decay_release_cycles(idx: u8) -> u32 {
    ATTACK_CYCLES[idx as usize & 0xf] * 3
}

// ── Noise LFSR helpers (VICE fastsid.c lines 85-91) ──────────────────────────

/// VICE NSHIFT(v, 16) — one step of the 23-bit noise LFSR.
#[inline]
fn nshift(v: u32) -> u32 {
    let n: u32 = 16;
    let a = v << n;
    let b = ((v >> (23 - n)) ^ (v >> (18 - n))) & ((1 << n) - 1);
    a | b
}

/// VICE NVALUE(v) — pack 8 bits from the LFSR to form the noise output byte.
///   bit 7: v[22]  bit 6: v[20]  bit 5: v[16]  bit 4: v[13]
///   bit 3: v[11]  bit 2: v[7]   bit 1: v[4]   bit 0: v[2]
#[inline]
fn nvalue(v: u32) -> u8 {
    (((v >> 22) & 1) << 7
        | ((v >> 20) & 1) << 6
        | ((v >> 16) & 1) << 5
        | ((v >> 13) & 1) << 4
        | ((v >> 11) & 1) << 3
        | ((v >> 7)  & 1) << 2
        | ((v >> 4)  & 1) << 1
        | ((v >> 2)  & 1)) as u8
}

// ── NSEED (VICE fastsid.c #define NSEED 0x7ffff8) ────────────────────────────
const NSEED: u32 = 0x7f_fff8;

// ── Voice-relative register offsets (VICE fastsid.c voice_t->d[0..6]) ────────
const V_FREQ_LO: usize = 0;
const V_FREQ_HI: usize = 1;
const V_PW_LO:   usize = 2;
const V_PW_HI:   usize = 3;
const V_CTRL:    usize = 4;
const V_AD:      usize = 5;
const V_SR:      usize = 6;

// ── Read-only register offsets (absolute, into the 32-byte tile) ─────────────
const SR_OSC3: usize = 0x1b;
const SR_ENV3: usize = 0x1c;

// ── Voice internal state ──────────────────────────────────────────────────────

/// Per-voice internal state. Field names mirror VICE fastsid.c for greppability.
#[derive(Clone, Debug)]
pub struct Voice {
    /// Phase counter, 24-bit (advanced per cycle by `fs`). VICE: `uint32_t f`.
    f: u32,
    /// Per-cycle frequency step (raw 16-bit from registers). VICE: `uint32_t fs`.
    fs: u32,
    /// Pulse width, 12-bit. VICE: `uint32_t pw`.
    pw: u32,
    /// Waveform-select high nibble of control register (bits 4..7 → 0..3).
    wt_select: u8,
    /// Noise flag (ctrl bit 7). VICE: `uint8_t noise`.
    noise: bool,
    /// Hard-sync enable (ctrl bit 1). VICE: `uint8_t sync`. Stored for future use.
    sync: bool,
    // ── ADSR ──────────────────────────────────────────────────────────────────
    /// 4-bit attack index. VICE: `uint8_t attack`.
    attack: u8,
    /// 4-bit decay index. VICE: `uint8_t decay`.
    decay: u8,
    /// 4-bit sustain level (0..15). VICE: `uint8_t sustain`.
    sustain: u8,
    /// 4-bit release index. VICE: `uint8_t release`.
    release: u8,
    /// Current ADSR state. VICE: `uint8_t adsrm`.
    pub adsrm: u8,
    /// Envelope value 0..255. VICE: `(adsr >> 23) & 0xff`.
    pub adsr_value: u8,
    /// Sub-cycle accumulator for the current ADSR step rate.
    cycle_accum: u32,
    /// Previous GATE bit (edge detection on V_CTRL write).
    prev_gate: bool,
    /// 23-bit noise LFSR. VICE: `uint32_t rv`.
    rv: u32,
}

impl Voice {
    fn new() -> Self {
        Self {
            f: 0,
            fs: 0,
            pw: 0,
            wt_select: 0,
            noise: false,
            sync: false,
            attack: 0,
            decay: 0,
            sustain: 0,
            release: 0,
            adsrm: ADSR_IDLE,
            adsr_value: 0,
            cycle_accum: 0,
            prev_gate: false,
            rv: NSEED,
        }
    }
}

// ── Sid6581 ───────────────────────────────────────────────────────────────────

/// SID 6581 oscillator + envelope model (B-level, no audio PCM).
///
/// Holds the three-voice internal state. The register file (raw write-only bytes
/// $D400-$D41C) lives in `Machine::sid_regs`; this struct reads from a slice of
/// that array on every register write and during `read_osc3` / `read_env3`.
///
/// Clone-able with the Machine for Phase-2 COW forks.
///
/// Spec 855 D4 — `Clone` and `Debug` are DERIVED again. They used to be written
/// by hand because this struct carried the audio `write_trace` hook, and a
/// `Box<dyn FnMut>` is neither. The hook has moved to `Machine` as [`SidTrace`]:
/// with several chips a subscriber wants to know WHICH one wrote, and installing
/// one hook per engine would mean re-installing on every firmware remap. The
/// engine is back to being only what the 6502 can see.
#[derive(Clone, Debug)]
pub struct Sid6581 {
    pub voices: [Voice; 3],
}

impl Sid6581 {
    /// Create at power-on defaults (voices zeroed, LFSR seeded with NSEED).
    pub fn new() -> Self {
        Self { voices: [Voice::new(), Voice::new(), Voice::new()] }
    }

    /// VICE: fastsid_reset() — clear all voice state to power-on defaults.
    /// Matches the TS sid.ts `reset()` implementation exactly. The audio hook is
    /// preserved across reset (it is a subscription, not register state — same
    /// as the TS `Resid.reset()` keeping its `writeTrace`).
    pub fn reset(&mut self) {
        self.voices = [Voice::new(), Voice::new(), Voice::new()];
    }

    // ── Register write dispatch ────────────────────────────────────────────────

    /// VICE: fastsid_store() — dispatch a write to the SID register file.
    ///
    /// `reg` = absolute SID register index (0x00..0x1f, already masked to &0x1f
    /// by the caller). `value` = byte to store. `regs` = the full 32-byte
    /// register shadow (Machine::sid_regs) so frequency/PW cross-reads work.
    ///
    /// Only voice registers 0x00..0x14 trigger state-machine updates; the
    /// filter/volume registers 0x15-0x18 and read-only 0x19-0x1f are no-ops here
    /// (caller stores the raw byte into the shadow; we model no filter audio).
    pub fn write(&mut self, reg: usize, value: u8, regs: &[u8; 32]) {
        // Spec 855 D4 — the audio subscriber used to be notified HERE. It now
        // lives on `Machine` as `SidTrace` and is fired by the paths that know
        // which chip they are writing to and at which cycle: the bus dispatch
        // and `poke_io`. This engine is once again only what the 6502 sees.
        match reg {
            0x00..=0x06 => self.apply_voice_write(0, reg, value, regs),
            0x07..=0x0d => self.apply_voice_write(1, reg - 7, value, regs),
            0x0e..=0x14 => self.apply_voice_write(2, reg - 14, value, regs),
            _ => { /* filter/vol/read-only: register shadow already updated by caller */ }
        }
    }

    /// VICE: fastsid_read() — computed reads for $D41B (osc3) and $D41C (env3).
    ///
    /// `reg` = absolute SID register index (already masked to &0x1f).
    /// `regs` = the 32-byte register shadow.
    ///
    /// Returns the live computed value for OSC3/ENV3; for all other registers
    /// returns the stored shadow byte (write-only on real HW; B-level round-trip).
    pub fn read(&self, reg: usize, regs: &[u8; 32]) -> u8 {
        match reg {
            0x19 => 0x80, // POT X unconnected (VICE default per Spec 429)
            0x1a => 0x80, // POT Y unconnected
            SR_OSC3 => self.read_osc3(regs),
            SR_ENV3 => self.voices[2].adsr_value,
            0x1d | 0x1e | 0x1f => 0, // unused/open-bus
            _ => regs[reg],
        }
    }

    // ── .c64re native snapshot (additive — ADR-077) ──────────────────────────
    //
    // Reads/writes the private per-voice state into/out of the c64re
    // `SidSnapshot.voices` shape (sid.ts:170-177). Pure state I/O — the byte-exact
    // fastsid register engine is unchanged. `gateflip` is always 0 at a snapshot
    // boundary (TRX64 acts on the GATE edge immediately at the V_CTRL write, like
    // the TS does — sid.ts:381 `vc.gateflip = 0`), so it is emitted as 0.

    /// Read voice `i` into the c64re `SidSnapshot.voices[i]` field tuple.
    /// Order matches sid.ts:170-176:
    /// (f, fs, pw, noise, wt_select, attack, decay, sustain, release, sync,
    ///  adsrm, adsr_value, cycle_accum, gateflip, prev_gate, rv).
    #[allow(clippy::type_complexity)]
    pub fn c64re_voice(&self, i: usize) -> (u32, u32, u32, u8, u8, u8, u8, u8, u8, u8, u8, u8, u32, u8, u8, u32) {
        let v = &self.voices[i];
        (
            v.f, v.fs, v.pw,
            v.noise as u8, v.wt_select,
            v.attack, v.decay, v.sustain, v.release,
            v.sync as u8,
            v.adsrm, v.adsr_value, v.cycle_accum,
            0, // gateflip — always 0 at a boundary
            v.prev_gate as u8, v.rv,
        )
    }

    /// Restore voice `i` from the c64re `SidSnapshot.voices[i]` field tuple.
    #[allow(clippy::too_many_arguments)]
    pub fn c64re_set_voice(
        &mut self, i: usize,
        f: u32, fs: u32, pw: u32, noise: u8, wt_select: u8,
        attack: u8, decay: u8, sustain: u8, release: u8, sync: u8,
        adsrm: u8, adsr_value: u8, cycle_accum: u32, _gateflip: u8, prev_gate: u8, rv: u32,
    ) {
        let v = &mut self.voices[i];
        v.f = f; v.fs = fs; v.pw = pw;
        v.noise = noise != 0; v.wt_select = wt_select;
        v.attack = attack; v.decay = decay; v.sustain = sustain; v.release = release;
        v.sync = sync != 0;
        v.adsrm = adsrm; v.adsr_value = adsr_value; v.cycle_accum = cycle_accum;
        v.prev_gate = prev_gate != 0; v.rv = rv;
    }

    // ── Tick (per-instruction wall-clock batch advance) ───────────────────────

    /// Advance SID state by `cycles` master-clock cycles.
    ///
    /// Ticked once per CPU instruction with the instruction's cycle cost —
    /// the same batched pattern as TS `integrated-session.ts:946 sid.tick(totalCycles)`.
    ///
    /// B-level scope:
    ///   - Voice-3 phase + LFSR advance (for $D41B osc3 readback).
    ///   - All-voices ADSR state machine.
    ///
    /// Audio PCM / sample generation is Phase-1.5 — NOT implemented.
    pub fn tick(&mut self, cycles: u64, regs: &[u8; 32]) {
        if cycles == 0 {
            return;
        }
        self.advance_voice3(cycles, regs);
        for i in 0..3 {
            Self::advance_adsr_for(&mut self.voices[i], cycles as u32);
        }
    }

    // ── Internal: voice register writes ───────────────────────────────────────

    /// VICE: fastsid_store() per-voice handling (fastsid.c lines 1133-1183).
    /// `rel` = 0..6 (voice-local offset). `regs` = full 32-byte shadow.
    fn apply_voice_write(&mut self, idx: usize, rel: usize, value: u8, regs: &[u8; 32]) {
        let base = idx * 7;
        let vc = &mut self.voices[idx];
        match rel {
            V_FREQ_LO | V_FREQ_HI => {
                // VICE setup_voice line 552: fs = freq16.
                vc.fs = (regs[base + V_FREQ_LO] as u32) | ((regs[base + V_FREQ_HI] as u32) << 8);
            }
            V_PW_LO | V_PW_HI => {
                // VICE setup_voice line 549: pw = (d[2] + (d[3] & 0x0f) * 256).
                vc.pw = (regs[base + V_PW_LO] as u32)
                    | (((regs[base + V_PW_HI] & 0x0f) as u32) << 8);
            }
            V_CTRL => {
                // VICE fastsid.c case 4/11/18 — gateflip tracking + voice update.
                let ctrl = value;
                vc.sync = (ctrl & 0x02) != 0;
                vc.wt_select = (ctrl >> 4) & 0x0f;
                vc.noise = (ctrl & 0x80) != 0;
                // TEST bit (ctrl & 0x08): VICE setup_voice lines 554-557 —
                // f = fs = 0, rv = NSEED.
                if ctrl & 0x08 != 0 {
                    vc.f = 0;
                    vc.fs = 0;
                    vc.rv = NSEED;
                } else {
                    // Restore fs from current freq registers.
                    vc.fs = (regs[base + V_FREQ_LO] as u32)
                        | ((regs[base + V_FREQ_HI] as u32) << 8);
                }
                // GATE-edge ADSR transitions — VICE setup_voice 660-678.
                let new_gate = (ctrl & 0x01) != 0;
                if new_gate && !vc.prev_gate {
                    // Rising edge → ATTACK.
                    vc.adsrm = ADSR_ATTACK;
                    vc.cycle_accum = 0;
                } else if !new_gate && vc.prev_gate {
                    // Falling edge → RELEASE.
                    vc.adsrm = ADSR_RELEASE;
                    vc.cycle_accum = 0;
                }
                vc.prev_gate = new_gate;
            }
            V_AD => {
                // VICE setup_voice line 544-545: attack = d[5] >> 4, decay = d[5] & 0x0f.
                vc.attack = (value >> 4) & 0x0f;
                vc.decay  = value & 0x0f;
            }
            V_SR => {
                // VICE setup_voice line 546-547: sustain = d[6] >> 4, release = d[6] & 0x0f.
                vc.sustain = (value >> 4) & 0x0f;
                vc.release = value & 0x0f;
            }
            _ => {}
        }
    }

    // ── Internal: voice-3 phase + LFSR advance ────────────────────────────────

    /// VICE: per-sample phase advance (fastsid_calculate_single_sample line 794+).
    /// Advances voice-3's 24-bit phase counter by `fs` per cycle; when the counter
    /// wraps (24-bit boundary), NSHIFT(rv, 16) advances the noise LFSR.
    ///
    /// Only voice-3 is advanced (for $D41B readback). Voices 1/2 audio oscillators
    /// are Phase-1.5.
    fn advance_voice3(&mut self, cycles: u64, regs: &[u8; 32]) {
        let vc = &mut self.voices[2];
        // TEST bit holds phase at 0.
        let ctrl = regs[14 + V_CTRL]; // voice-3 ctrl reg = base 14 + V_CTRL 4 = regs[18]
        if ctrl & 0x08 != 0 {
            return;
        }
        let fs = vc.fs;
        if fs == 0 {
            // With fs=0 phase never wraps; LFSR stays static. Match TS behavior.
            return;
        }
        // Per-cycle loop (B-level; typical freq values wrap < 1× per cycle for
        // audio, occasionally more for test exercisers — iteration is bounded).
        for _ in 0..cycles {
            let before = vc.f;
            let next = (before + fs) & 0x00ff_ffff; // 24-bit mask
            vc.f = next;
            // 24-bit wrap: new < old (with 24-bit mask, wrap is detected by
            // checking if we passed through 0: old + fs >= 0x1000000).
            if before.wrapping_add(fs) >= 0x0100_0000 {
                vc.rv = nshift(vc.rv);
            }
        }
    }

    // ── Internal: osc3 readback ───────────────────────────────────────────────

    /// VICE: doosc() (fastsid.c line 341) — 8-bit waveform output for $D41B.
    /// Wave shapes are ANDed when multiple bits are set (combined waveforms).
    fn read_osc3(&self, regs: &[u8; 32]) -> u8 {
        let vc = &self.voices[2];
        let ctrl = regs[14 + V_CTRL]; // regs[18]
        let wave = (ctrl >> 4) & 0x0f;
        if wave == 0 {
            return 0;
        }
        let mut out: u8 = 0xff;
        let mut any = false;
        // Triangle (bit 4).
        if ctrl & 0x10 != 0 {
            // Spec 151 line 58: tri_out = (phase >> 11) ^ (if phase & 0x800000 { 0xfff } else { 0 })
            // take high 8 bits of 12-bit result.
            let tri12 = ((vc.f >> 11) ^ (if vc.f & 0x800000 != 0 { 0xfff } else { 0 })) & 0xfff;
            out &= ((tri12 >> 4) & 0xff) as u8;
            any = true;
        }
        // Sawtooth (bit 5).
        if ctrl & 0x20 != 0 {
            // Spec 151 line 59: (phase >> 16) & 0xff.
            out &= ((vc.f >> 16) & 0xff) as u8;
            any = true;
        }
        // Pulse (bit 6).
        if ctrl & 0x40 != 0 {
            // Spec 151 line 60: phase < (pulsewidth << 12) ? 0xff : 0.
            // pw is 12-bit; (pw << 12) is in the 24-bit phase domain.
            let pw_shifted = (vc.pw << 12) & 0x00ff_ffff;
            out &= if vc.f < pw_shifted { 0x00 } else { 0xff };
            any = true;
        }
        // Noise (bit 7).
        if ctrl & 0x80 != 0 {
            out &= nvalue(vc.rv);
            any = true;
        }
        if any { out } else { 0 }
    }

    // ── Internal: ADSR state machine ──────────────────────────────────────────

    /// VICE: trigger_adsr() / set_adsr() (fastsid.c 387-450) — advance one voice's
    /// ADSR envelope by `cycles` master-clock cycles.
    fn advance_adsr_for(vc: &mut Voice, mut cycles: u32) {
        loop {
            match vc.adsrm {
                ADSR_IDLE => {
                    vc.adsr_value = 0;
                    return;
                }
                ADSR_SUSTAIN => {
                    // Hold at sustain level. Recompute in case sustain nibble changed.
                    vc.adsr_value = vc.sustain.saturating_mul(17);
                    return;
                }
                ADSR_ATTACK => {
                    let rate = ATTACK_CYCLES[vc.attack as usize & 0xf];
                    // saturating: the program can shrink the rate nibble mid-envelope
                    // so `rate < cycle_accum`; the boundary is then already passed
                    // (need=0 → step fires now), never an underflow.
                    let need = rate.saturating_sub(vc.cycle_accum);
                    if cycles < need {
                        vc.cycle_accum += cycles;
                        return;
                    }
                    cycles -= need;
                    vc.cycle_accum = 0;
                    if vc.adsr_value < 0xff {
                        vc.adsr_value += 1;
                    }
                    if vc.adsr_value == 0xff {
                        // ATTACK → DECAY.
                        vc.adsrm = ADSR_DECAY;
                        vc.cycle_accum = 0;
                    }
                }
                ADSR_DECAY => {
                    let rate = decay_release_cycles(vc.decay);
                    let need = rate.saturating_sub(vc.cycle_accum);
                    if cycles < need {
                        vc.cycle_accum += cycles;
                        return;
                    }
                    cycles -= need;
                    vc.cycle_accum = 0;
                    let sustain_level = vc.sustain.saturating_mul(17);
                    if vc.adsr_value <= sustain_level {
                        vc.adsrm = ADSR_SUSTAIN;
                        vc.adsr_value = sustain_level;
                        return;
                    }
                    vc.adsr_value -= 1;
                }
                ADSR_RELEASE => {
                    let rate = decay_release_cycles(vc.release);
                    let need = rate.saturating_sub(vc.cycle_accum);
                    if cycles < need {
                        vc.cycle_accum += cycles;
                        return;
                    }
                    cycles -= need;
                    vc.cycle_accum = 0;
                    if vc.adsr_value == 0 {
                        vc.adsrm = ADSR_IDLE;
                        return;
                    }
                    vc.adsr_value -= 1;
                }
                _ => return,
            }
        }
    }
}

impl Default for Sid6581 {
    fn default() -> Self {
        Self::new()
    }
}

// ── Spec 855 — more than one SID ──────────────────────────────────────────────
//
// A U64-II decodes four SID targets and its firmware assigns their addresses at
// runtime, rewriting them on every C64 reset. So the core carries N chips and a
// table that says which address belongs to which, and it learns nothing about
// the host's registers: the host RESOLVES its own hardware into `SidMapping`s
// and hands them over. No VICE defaults are baked in here — an empty table means
// exactly the pre-855 machine, `$D400-$D7FF` mirrored onto chip 0.

/// One SID: its 32-byte register file and the oscillator/envelope model that
/// answers `$D41B`/`$D41C` for it.
///
/// Chip 0 is NOT one of these. It stays `Machine::sid_regs` + `Machine::sid`,
/// where every existing path — the snapshot, the VSF export, the monitor, the
/// bus — already names it. Making it an element of a list would force all of
/// them to index for no behavioural gain, and 855 D3 asks for chip 0 to be
/// bit-identical to before rather than merely equivalent.
#[derive(Clone, Debug, Default)]
pub struct SidChip {
    /// Register shadow, the same 32 write-only bytes chip 0 keeps in `sid_regs`.
    pub regs: [u8; 32],
    /// Oscillator + envelope state. `$D41B`/`$D41C` are per chip, which is the
    /// whole reason an extra chip needs a model at all and not just storage.
    pub engine: Sid6581,
}

impl SidChip {
    pub fn new() -> Self {
        Self { regs: [0u8; 32], engine: Sid6581::new() }
    }

    /// Power-on: register file cleared, voice state reset. Matches what
    /// `cold_reset` does to chip 0.
    pub fn reset(&mut self) {
        self.regs = [0u8; 32];
        self.engine.reset();
    }
}

/// One decoded window: `start..=end` belongs to `chip`.
///
/// The host builds these. On a U64 that means resolving `>>4` base registers,
/// the A11..A4 masks, the enable bits, `EMUSID_SPLIT`/`ADDRSEL` and
/// `0x01 = unmapped` — none of which this crate knows or wants to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidMapping {
    /// First address of the window, inclusive.
    pub start: u16,
    /// Last address of the window, inclusive.
    pub end: u16,
    /// 0 = the machine's own SID; 1.. = `Machine::sid_extra[chip - 1]`.
    pub chip: u8,
    /// Spec 855 §7, and deliberately WITHOUT a default. A window in
    /// `$DE00-$DFFF` shares the address space with the expansion chain — a
    /// cartridge, the UCI, the REU's mirror, a host's sampler — and whether the
    /// SID decode sits ahead of that chain or behind it is not something either
    /// side of this interface can currently demonstrate: the U64-II top level
    /// that would settle it is not in the open firmware tree, and the open U2+
    /// top has no UltiSID. So the host states it per window and can match the
    /// RTL when someone reads it. Hardcoding either guess here would repeat the
    /// UCI pointer defect, which was an unverified assumption whose own gate
    /// confirmed it.
    pub ahead_of_expansion: bool,
}

impl SidMapping {
    /// A 32-byte window at `start` for `chip`, the shape a SID actually occupies.
    pub fn window(start: u16, chip: u8, ahead_of_expansion: bool) -> Self {
        Self { start, end: start.wrapping_add(0x1f), chip, ahead_of_expansion }
    }

    #[inline]
    pub fn contains(&self, addr: u16) -> bool {
        addr >= self.start && addr <= self.end
    }

    /// The register this address hits within the window. Masked to 0x00..0x1f,
    /// so a window wider than 32 bytes mirrors, exactly as a real SID does
    /// across `$D400-$D7FF`.
    #[inline]
    pub fn reg(&self, addr: u16) -> usize {
        (addr.wrapping_sub(self.start) as usize) & 0x1f
    }
}

/// Resolve an address against the table: `(chip, register)`, or `None` when no
/// window claims it.
///
/// First match wins, so a host that overlaps two windows gets the order it gave
/// rather than a rule invented here. An empty table resolves nothing, which is
/// what keeps a stock machine on the pre-855 path.
#[inline]
pub fn resolve_sid(map: &[SidMapping], addr: u16) -> Option<(u8, usize)> {
    map.iter().find(|m| m.contains(addr)).map(|m| (m.chip, m.reg(addr)))
}

#[cfg(test)]
mod spec855_tests {
    use super::*;

    #[test]
    fn an_empty_table_claims_nothing() {
        assert_eq!(resolve_sid(&[], 0xd400), None);
    }

    #[test]
    fn a_window_resolves_to_its_chip_and_register() {
        let map = [SidMapping::window(0xd400, 0, true), SidMapping::window(0xde00, 1, false)];
        assert_eq!(resolve_sid(&map, 0xd400), Some((0, 0x00)));
        assert_eq!(resolve_sid(&map, 0xd41b), Some((0, 0x1b)));
        assert_eq!(resolve_sid(&map, 0xde04), Some((1, 0x04)));
        assert_eq!(resolve_sid(&map, 0xdf00), None, "an address nobody claimed");
    }

    #[test]
    fn a_wide_window_mirrors_every_thirty_two_bytes() {
        // The U64's A11..A4 mask can open a 256-byte window; a SID has 32
        // registers, so the rest is mirror — the same thing $D400-$D7FF does.
        let wide = SidMapping { start: 0xde00, end: 0xdeff, chip: 2, ahead_of_expansion: false };
        assert_eq!(resolve_sid(&[wide], 0xde00), Some((2, 0x00)));
        assert_eq!(resolve_sid(&[wide], 0xde20), Some((2, 0x00)), "mirrored");
        assert_eq!(resolve_sid(&[wide], 0xde3b), Some((2, 0x1b)));
    }

    #[test]
    fn the_first_matching_window_wins() {
        let map = [SidMapping::window(0xde00, 1, false), SidMapping::window(0xde00, 3, false)];
        assert_eq!(resolve_sid(&map, 0xde05), Some((1, 0x05)), "the order the host gave");
    }
}

// ── Spec 855 D4 — the write trace ─────────────────────────────────────────────

/// The audio subscriber: every SID register write, in CPU order, as
/// `(chip, reg, value, clk)`.
///
/// Until 855 this was `FnMut(u8, u8)` living on `Sid6581` — register and value,
/// with neither the chip nor the cycle. With one SID that was enough. With
/// several it is not: a host cannot tell which engine to clock, which is UE2's
/// "gap 3", and it had been pairing the hook with an observer just to recover
/// the cycle.
///
/// The ADDRESS is deliberately not passed. The host built the decode table, so
/// chip plus register gives it back, and passing both invites them to disagree.
///
/// WHY IT LIVES ON THE MACHINE AND NOT ON THE ENGINE. Per engine, a host would
/// have to install N hooks and re-install them every time the firmware remaps —
/// and `Sid6581::write` is reached from the bus, from `poke_io` and from the
/// isolated `SidBus`, so moving the call to the bus dispatch instead would
/// silently stop tracing host pokes. A monitor write to `$D418` would go quiet
/// without anything reporting it. One hook, at the machine, fired by every path
/// that reaches a register file.
///
/// A newtype rather than a bare field because `Box<dyn FnMut>` is not `Clone`
/// and `Machine`'s `#[derive(Clone)]` is load-bearing — it is the COW fork base.
/// Cloning drops the subscriber, exactly as `Sid6581` already does: a fork
/// starts audio-silent, which is the honest default for a branch nobody is
/// listening to.
#[derive(Default)]
pub struct SidTrace(pub Option<Box<dyn FnMut(u8, u8, u8, u64) + Send>>);

impl SidTrace {
    /// Fire the hook if one is installed. Zero cost when `None`.
    #[inline]
    pub fn fire(&mut self, chip: u8, reg: usize, value: u8, clk: u64) {
        if let Some(hook) = self.0.as_mut() {
            hook(chip, (reg as u8) & 0x1f, value, clk);
        }
    }

    #[inline]
    pub fn is_installed(&self) -> bool {
        self.0.is_some()
    }
}

impl Clone for SidTrace {
    /// Transport plumbing, not register state: a clone carries no subscriber.
    fn clone(&self) -> Self {
        Self(None)
    }
}

impl core::fmt::Debug for SidTrace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("SidTrace").field(&self.0.as_ref().map(|_| "Some(<fn>)")).finish()
    }
}

// ── Spec 855 D5 — a host may answer for a chip ────────────────────────────────

/// Read and peek overrides, so a host can answer for a chip the core does not
/// model.
///
/// UE2 needs this because an emulated ARMSID in its configuration mode must
/// answer `$1B`/`$1C` from its own protocol rather than from `Sid6581`. Today
/// only the firmware probes that, over DMA, which the bridge answers itself — a
/// C64 program probing through the CPU would read our fastsid and get it wrong.
///
/// ONE hook pair for the machine, taking the chip as a parameter, rather than a
/// pair per chip: the same reason D4's write trace lives here. Per chip, a host
/// would have to install N of them and re-install on every firmware remap.
///
/// `Some` wins and `None` falls through to the core, exactly as
/// `ExpansionDevice::read` behaves under Spec 850 — one idiom in this codebase
/// rather than a second one invented here.
///
/// THE TWO HALVES HAVE DIFFERENT BOUNDS, and that is the contract showing
/// through the types. `read` is `FnMut` because the bus dispatch holds `&mut
/// self` and a real read may advance the host's own protocol state. `peek` is
/// `Fn`, because `Machine::read_full` and `peek_lens` take `&self`: a peek is
/// side-effect-free by definition, so being unable to mutate is not a
/// restriction, it is the rule enforced.
///
/// Without the peek half the monitor would show the core's register shadow
/// while the C64 receives the host's answer. A quiet disagreement between what
/// a debugger prints and what the program reads is the kind of thing that costs
/// somebody an afternoon, which is why 850 grew the same pair.
#[derive(Default)]
pub struct SidHostAccess {
    /// Answer a bus READ of `(chip, reg)`, or fall through with `None`.
    pub read: Option<Box<dyn FnMut(u8, usize) -> Option<u8> + Send>>,
    /// Answer a side-effect-free PEEK of `(chip, reg)`, or fall through.
    pub peek: Option<Box<dyn Fn(u8, usize) -> Option<u8> + Send + Sync>>,
}

impl SidHostAccess {
    #[inline]
    pub fn read(&mut self, chip: u8, reg: usize) -> Option<u8> {
        self.read.as_mut().and_then(|f| f(chip, reg))
    }

    #[inline]
    pub fn peek(&self, chip: u8, reg: usize) -> Option<u8> {
        self.peek.as_ref().and_then(|f| f(chip, reg))
    }

    #[inline]
    pub fn is_installed(&self) -> bool {
        self.read.is_some() || self.peek.is_some()
    }
}

impl Clone for SidHostAccess {
    /// Transport plumbing, not register state: a fork answers from the core.
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl core::fmt::Debug for SidHostAccess {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SidHostAccess")
            .field("read", &self.read.as_ref().map(|_| "Some(<fn>)"))
            .field("peek", &self.peek.as_ref().map(|_| "Some(<fn>)"))
            .finish()
    }
}
