//! Spec 859 — the raster line as the VIC saw it.
//!
//! A cycle-by-cycle record of what the VIC-II fetched in each half-cycle, whether BA and AEC
//! were down, what its internal counters held, and what the CPU did on the bus in the same
//! cycle. It is written by the chip itself (`VicII::tick` calls into [`VicCycleRecorder`] at
//! two points when one is armed) and by an [`Observer`] on the CPU side. Nothing here computes
//! timing: every field is a value the core produced. A timing model next to the core would be
//! a second VIC, and the two would disagree exactly where it matters.
//!
//! The recorder is armed only inside a scratch replay ([`record_frame`]), and the hooks are
//! compiled only into the VIC tick a recording observer instantiates
//! (`Observer::RECORDS_VIC`): the live path's `tick` contains no recorder code.

use crate::{BusKind, Machine, Observer};
use serde_json::{json, Value};

/// Cycles per PAL line / lines per PAL frame (the 6569 is the only chip the core models).
pub const CYCLES_PER_LINE: u64 = 63;
pub const LINES_PER_FRAME: u64 = 312;
pub const CYCLES_PER_FRAME: u64 = CYCLES_PER_LINE * LINES_PER_FRAME;

/// Upper bound on records one arming may collect (three frames). A replay that overruns it
/// stops recording rather than growing without limit.
const MAX_RECORDS: usize = 3 * CYCLES_PER_FRAME as usize;

/// What the VIC did in the first half of the cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phi1Kind {
    /// The idle access (`$3FFF`) — nothing of use is read.
    #[default]
    Idle,
    /// DRAM refresh (`$3F00 + refresh counter`).
    Refresh,
    /// g-access in the display state.
    Graphics,
    /// g-access in the idle state (`$3FFF`, `$39FF` with ECM).
    IdleGraphics,
    /// p-access: sprite pointer.
    SpritePointer,
    /// s-access: sprite data (DMA on).
    SpriteData,
}

/// What happened in the second half of the cycle: the CPU's half, unless the VIC took it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phi2Kind {
    #[default]
    Cpu,
    /// c-access: video matrix + colour RAM (bad line).
    Matrix,
    /// s-access: sprite data (DMA on).
    SpriteData,
}

/// One PHI2 cycle.
#[derive(Clone, Copy, Debug, Default)]
pub struct VicCycle {
    /// The machine clock of this cycle. CPU accesses stamped with the same clk happened in
    /// this cycle's second half.
    pub clk: u64,
    /// The line the cycle belongs to. Equal to the raster counter except in cycle 1 of line
    /// 0, where the counter still reads 311 (it resets in cycle 2).
    pub line: u16,
    /// The raster counter (`$D012` + bit 8 of `$D011`) as the chip holds it.
    pub raster: u16,
    /// 1..=63, as in Bauer's and Åkesson's tables.
    pub cycle: u8,

    pub phi1: Phi1Kind,
    pub phi1_sprite: u8,
    /// The address on the bus, bank-absolute (`vbank + 14-bit VIC address`).
    pub phi1_addr: u16,
    /// The access hit the character ROM (banks 0 and 2, `$1000-$1FFF`).
    pub phi1_rom: bool,
    pub phi1_data: u8,

    pub phi2: Phi2Kind,
    pub phi2_sprite: u8,
    pub phi2_addr: u16,
    pub phi2_data: u8,
    /// The VIC wanted the half-cycle but AEC was still high (the three cycles after BA
    /// drops): the CPU kept the bus, the VIC read `$FF`.
    pub phi2_blocked: bool,

    /// BA low — the VIC is taking the bus. A CPU read stalls; a write still completes.
    pub ba: bool,
    /// AEC low in the second half — the VIC owns the address bus.
    pub aec: bool,

    pub bad_line: bool,
    pub idle: bool,
    pub vc: u16,
    pub vcbase: u16,
    pub rc: u8,
    pub vmli: u16,
    pub sprite_dma: u8,
    pub sprite_display: u8,
    pub mc: [u8; 8],
    pub mcbase: [u8; 8],
    pub main_border: bool,
    pub vertical_border: bool,

    /// Where this cycle's draw put its 8 pixels: framebuffer column and line (520 × 312).
    /// The draw runs one cycle behind the fetch — this is what was drawn, not what was
    /// fetched.
    pub fb_x: u16,
    pub fb_line: u16,

    pub d011: u8,
    pub d016: u8,
    pub d018: u8,
    pub vbank: u16,
}

/// The Φ1 access a cycle is about to make, taken BEFORE the fetch (the fetch advances VC,
/// VMLI and MC, and the address is formed from the values before).
#[derive(Clone, Copy, Debug, Default)]
pub struct Phi1Access {
    pub kind: Phi1Kind,
    pub sprite: u8,
    pub addr: u16,
}

/// Armed on `VicII::line_rec`. Collects one [`VicCycle`] per `tick`.
#[derive(Clone, Debug, Default)]
pub struct VicCycleRecorder {
    /// Clock of the last recorded tick. Set to the machine clock at arming; each tick is
    /// one PHI2 cycle, so it tracks `c64_core.clk` exactly (a gate checks that).
    pub clk: u64,
    pub cycles: Vec<VicCycle>,
    pub(crate) pending_phi1: Phi1Access,
    /// The previous cycle's flags: its Φ2 sprite fetch runs at the top of the next tick.
    pub(crate) prev_flags: u32,
    pub overflowed: bool,
}

impl VicCycleRecorder {
    pub fn armed_at(clk: u64) -> Self {
        VicCycleRecorder { clk, cycles: Vec::with_capacity(CYCLES_PER_FRAME as usize + 1024), ..Default::default() }
    }

    #[inline]
    pub(crate) fn push(&mut self, mut c: VicCycle) {
        self.clk = self.clk.wrapping_add(1);
        c.clk = self.clk;
        if self.cycles.len() < MAX_RECORDS {
            self.cycles.push(c);
        } else {
            self.overflowed = true;
        }
    }
}

/// One CPU bus access.
#[derive(Clone, Copy, Debug)]
pub struct CpuAccess {
    pub clk: u64,
    pub kind: BusKind,
    pub addr: u16,
    pub value: u8,
}

/// One retired instruction, placed on the clock by its opcode fetch.
#[derive(Clone, Copy, Debug)]
pub struct Instruction {
    pub pc: u16,
    pub opcode: u8,
    pub b1: u8,
    pub b2: u8,
    /// Clock of the opcode fetch.
    pub start: u64,
    /// Clock at which the instruction had retired (the observer's post-instruction clk).
    pub end: u64,
    /// An interrupt was taken before this instruction: the accesses between the previous
    /// instruction and this opcode fetch are the interrupt sequence.
    pub after_interrupt: Option<u16>,
    /// With `after_interrupt`: the clk of the sequence's first access.
    pub interrupt_start: Option<u64>,
}

/// The CPU half of the record: every bus access and every instruction boundary.
#[derive(Default)]
pub struct LineTraceObserver {
    pub accesses: Vec<CpuAccess>,
    pub instructions: Vec<Instruction>,
    /// Index into `accesses` where the current instruction's accesses begin.
    mark: usize,
    pending_interrupt: Option<u16>,
}

impl Observer for LineTraceObserver {
    const RECORDS_VIC: bool = true;

    fn on_instruction(&mut self, pc: u16, opcode: u8, b1: u8, b2: u8, _a: u8, _x: u8, _y: u8, _sp: u8, _p: u8, clk: u64) {
        // The opcode fetch is the first Fetch of `pc` since the previous instruction. An
        // interrupt sequence (pushes, vector reads) sits before it inside the same call.
        let start = self.accesses[self.mark..]
            .iter()
            .find(|a| a.kind == BusKind::Fetch && a.addr == pc)
            .map(|a| a.clk)
            .unwrap_or(clk);
        self.instructions.push(Instruction {
            pc,
            opcode,
            b1,
            b2,
            start,
            end: clk,
            interrupt_start: self.pending_interrupt.map(|_| self.accesses.get(self.mark).map(|a| a.clk).unwrap_or(start)),
            after_interrupt: self.pending_interrupt.take(),
        });
        self.mark = self.accesses.len();
    }

    fn on_bus(&mut self, kind: BusKind, addr: u16, value: u8, _pc: u16, clk: u64, _old: u8) {
        if self.accesses.len() < MAX_RECORDS * 4 {
            self.accesses.push(CpuAccess { clk, kind, addr, value });
        }
    }

    fn on_interrupt(&mut self, vector: u16, _clk: u64) {
        self.pending_interrupt = Some(vector);
    }
}

/// Which frame a record is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameWhich {
    /// The frame on screen (`vic.displayed`) at the checkpoint.
    Displayed,
    /// The frame after it — used when no ring anchor reaches back far enough.
    Next,
}

/// A recorded frame: 19656 cycles, the CPU's accesses and instructions over them.
pub struct LineTraceFrame {
    pub which: FrameWhich,
    /// First clk of the frame (line 0, cycle 1).
    pub start_clk: u64,
    /// The replay drew the same picture the checkpoint shows. `None` when there was
    /// nothing to compare with (the `Next` frame).
    pub verified: Option<bool>,
    /// The anchor the replay started from, and how many cycles it ran before recording.
    pub anchor_clk: u64,
    pub cycles: Vec<VicCycle>,
    pub accesses: Vec<CpuAccess>,
    pub instructions: Vec<Instruction>,
}

/// Where the VIC stands, as a position in its frame: `line * 63 + (cycle - 1)`, with cycle 1
/// of line 0 counted to line 0 (the raster counter resets one cycle late).
pub fn frame_position(m: &Machine) -> u64 {
    let v = &m.vic;
    let line = if v.raster_cycle == 0 && v.start_of_frame { 0 } else { v.raster_line as u64 };
    line * CYCLES_PER_LINE + v.raster_cycle as u64
}

/// First clk of the frame the machine's `vic.displayed` holds.
///
/// The picture is published when the raster counter resets (cycle 2 of line 0): from then on
/// `displayed` is the frame that just ended. At cycle 1 of line 0 the reset has not happened,
/// so the picture is still the one before.
pub fn displayed_frame_start(m: &Machine) -> u64 {
    let clk = m.c64_core.clk;
    let pos = frame_position(m);
    let current_start = clk.wrapping_sub(pos);
    let back = if pos >= 1 { CYCLES_PER_FRAME } else { 2 * CYCLES_PER_FRAME };
    current_start.wrapping_sub(back)
}

/// Record the frame starting at `start_clk`. `m` is a SCRATCH machine standing at or before
/// `start_clk` (restored from an anchor); it is run forward and left past the frame.
///
/// `reference`, when given, is the picture the frame should produce (the checkpoint's
/// `displayed`, 520 × 312 colour indices); it is compared with what the replay publishes.
pub fn record_frame(
    m: &mut Machine,
    start_clk: u64,
    which: FrameWhich,
    reference: Option<&[u8]>,
) -> Result<LineTraceFrame, String> {
    const LEAD: u64 = 300;
    let anchor_clk = m.c64_core.clk;
    if anchor_clk > start_clk {
        return Err(format!("the replay starts at cycle {anchor_clk}, after the frame (cycle {start_clk})"));
    }
    let mut null = crate::NullSink;
    let gap = start_clk - anchor_clk;
    if gap > LEAD {
        m.run_for_full(gap - LEAD, &mut null, |_, _, _, _, _, _, _| {});
    }
    if m.c64_core.clk > start_clk {
        return Err(format!(
            "the replay overran the frame start ({} > {start_clk}) before recording began",
            m.c64_core.clk
        ));
    }

    let mut obs = LineTraceObserver::default();
    m.vic.line_rec = Some(Box::new(VicCycleRecorder::armed_at(m.c64_core.clk)));
    let end = start_clk + CYCLES_PER_FRAME;
    let budget = end + LEAD - m.c64_core.clk;
    m.run_for_full(budget, &mut obs, |_, _, _, _, _, _, _| {});
    let rec = m.vic.line_rec.take().expect("recorder armed above");

    if rec.clk != m.c64_core.clk {
        return Err(format!(
            "recorder clock {} disagrees with the machine's {} — the record cannot be placed",
            rec.clk, m.c64_core.clk
        ));
    }
    let cycles: Vec<VicCycle> =
        rec.cycles.into_iter().filter(|c| c.clk >= start_clk && c.clk < end).collect();
    match cycles.first() {
        Some(c) if c.line == 0 && c.cycle == 1 && cycles.len() == CYCLES_PER_FRAME as usize => {}
        Some(c) => {
            return Err(format!(
                "the frame does not start where it should: first recorded cycle is line {} cycle {} ({} cycles)",
                c.line,
                c.cycle,
                cycles.len()
            ))
        }
        None => return Err("nothing was recorded".into()),
    }
    // Compared over the visible window only (384 × 272): the draw never writes the
    // framebuffer's last 16 columns, so outside the window both hold whatever they held.
    let verified = reference.map(|r| {
        crate::render::index_buffer_to_canvas_indices(r).2
            == crate::render::index_buffer_to_canvas_indices(&m.vic.displayed[..]).2
    });
    let accesses: Vec<CpuAccess> = obs.accesses.into_iter().filter(|a| a.clk >= start_clk && a.clk < end).collect();
    let instructions: Vec<Instruction> =
        obs.instructions.into_iter().filter(|i| i.end > start_clk && i.start < end).collect();
    Ok(LineTraceFrame { which, start_clk, verified, anchor_clk, cycles, accesses, instructions })
}

fn phi1_name(k: Phi1Kind) -> &'static str {
    match k {
        Phi1Kind::Idle => "i",
        Phi1Kind::Refresh => "r",
        Phi1Kind::Graphics => "g",
        Phi1Kind::IdleGraphics => "gi",
        Phi1Kind::SpritePointer => "p",
        Phi1Kind::SpriteData => "s",
    }
}

fn phi2_name(k: Phi2Kind) -> &'static str {
    match k {
        Phi2Kind::Cpu => "cpu",
        Phi2Kind::Matrix => "c",
        Phi2Kind::SpriteData => "s",
    }
}

fn access_name(k: BusKind) -> &'static str {
    match k {
        BusKind::Fetch => "f",
        BusKind::Read => "r",
        BusKind::Write => "w",
        BusKind::DummyRead => "dr",
        BusKind::DummyWrite => "dw",
    }
}

impl LineTraceFrame {
    /// Lines `from..=to` as the wire shape. `text` formats an instruction (the daemon owns
    /// the disassembler).
    pub fn lines_json(&self, from: u16, to: u16, text: impl Fn(&Instruction) -> String) -> Value {
        let mut out = Vec::new();
        for line in from..=to.min(LINES_PER_FRAME as u16 - 1) {
            let base = line as usize * CYCLES_PER_LINE as usize;
            let Some(row) = self.cycles.get(base..base + CYCLES_PER_LINE as usize) else { continue };
            let (lo, hi) = (row[0].clk, row[row.len() - 1].clk);
            let cycles: Vec<Value> = row
                .iter()
                .map(|c| {
                    let cpu: Vec<Value> = self
                        .accesses
                        .iter()
                        .filter(|a| a.clk == c.clk)
                        .map(|a| json!({ "k": access_name(a.kind), "a": a.addr, "v": a.value }))
                        .collect();
                    json!({
                        "c": c.cycle,
                        "clk": c.clk,
                        "raster": c.raster,
                        "phi1": { "k": phi1_name(c.phi1), "spr": c.phi1_sprite, "a": c.phi1_addr, "rom": c.phi1_rom, "v": c.phi1_data },
                        "phi2": { "k": phi2_name(c.phi2), "spr": c.phi2_sprite, "a": c.phi2_addr, "v": c.phi2_data, "blocked": c.phi2_blocked },
                        "ba": c.ba,
                        "aec": c.aec,
                        "cpu": cpu,
                        "badLine": c.bad_line,
                        "idle": c.idle,
                        "vc": c.vc,
                        "vcbase": c.vcbase,
                        "rc": c.rc,
                        "vmli": c.vmli,
                        "spriteDma": c.sprite_dma,
                        "spriteDisplay": c.sprite_display,
                        "mc": c.mc,
                        "mcbase": c.mcbase,
                        "mainBorder": c.main_border,
                        "vBorder": c.vertical_border,
                        "fbX": c.fb_x,
                        "fbLine": c.fb_line,
                        "d011": c.d011,
                        "d016": c.d016,
                        "d018": c.d018,
                        "vbank": c.vbank,
                    })
                })
                .collect();
            let instructions: Vec<Value> = self
                .instructions
                .iter()
                .filter(|i| i.end > lo && i.start <= hi)
                .map(|i| {
                    json!({
                        "pc": i.pc,
                        "bytes": [i.opcode, i.b1, i.b2],
                        "text": text(i),
                        "start": i.start,
                        "end": i.end,
                        "interrupt": i.after_interrupt,
                        "interruptStart": i.interrupt_start,
                    })
                })
                .collect();
            out.push(json!({ "line": line, "badLine": row.iter().any(|c| c.bad_line), "cycles": cycles, "instructions": instructions }));
        }
        json!(out)
    }

    pub fn header_json(&self) -> Value {
        json!({
            "which": match self.which { FrameWhich::Displayed => "displayed", FrameWhich::Next => "next" },
            "startClk": self.start_clk,
            "verified": self.verified,
            "anchorClk": self.anchor_clk,
            "replayedCycles": self.start_clk.saturating_sub(self.anchor_clk),
            "chip": "6569",
            "cyclesPerLine": CYCLES_PER_LINE,
            "linesPerFrame": LINES_PER_FRAME,
            "fbOrigin": { "x": crate::render::CANVAS_X0, "y": crate::render::CANVAS_Y0 },
        })
    }
}
