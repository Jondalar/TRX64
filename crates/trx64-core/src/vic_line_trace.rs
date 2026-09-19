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

use crate::model::{DisplayWindow, MAX_CYCLES_PER_LINE, MAX_RASTER_LINES};
use crate::vic::LineGeometry;
use crate::{BusKind, Machine, Observer};
use serde_json::{json, Value};

// Spec 863 — the frame's geometry is the machine's model (`Machine::timing`): 63 × 312 on
// PAL, 65 × 263 on NTSC. Nothing below counts in literal PAL cycles.

/// Upper bound on records one arming may collect (three of the longest frames a model has).
/// A replay that overruns it stops recording rather than growing without limit.
const MAX_RECORDS: usize = 3 * MAX_CYCLES_PER_LINE * MAX_RASTER_LINES;

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
    /// 0, where the counter still reads the frame's last line (it resets in cycle 2).
    pub line: u16,
    /// The raster counter (`$D012` + bit 8 of `$D011`) as the chip holds it.
    pub raster: u16,
    /// 1..=cycles per line (63 PAL, 65 NTSC), as in Bauer's and Åkesson's tables.
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

    /// Where this cycle's draw put its 8 pixels: framebuffer column and row (520 × 312; a
    /// wrapped NTSC window draws lines 0-11 into rows 263-274).
    /// The draw runs one cycle behind the fetch — this is what was drawn, not what was
    /// fetched.
    pub fb_x: u16,
    pub fb_line: u16,

    pub d011: u8,
    pub d016: u8,
    pub d018: u8,
    pub vbank: u16,

    /// Spec 860 — for a display-state g-access: the cell it belongs to (VC before the fetch
    /// advances it) and the cell's screen byte and colour-RAM nibble, as the fetch used them.
    /// `phi1_data` is the graphics byte itself.
    pub g_vc: u16,
    pub g_char: u8,
    pub g_color: u8,
}

/// Spec 860 — the sprite registers of one line, taken at cycle 20 (inside the line, after the
/// pointer fetches that serve it — `LineGeometry::sample_cycle`).
#[derive(Clone, Copy, Debug, Default)]
pub struct LineSprites {
    pub clk: u64,
    pub line: u16,
    /// 9-bit X per sprite.
    pub x: [u16; 8],
    pub pointer: [u8; 8],
    pub color: [u8; 8],
    pub mc: u8,
    pub x_expand: u8,
    pub y_expand: u8,
}

/// The Φ1 access a cycle is about to make, taken BEFORE the fetch (the fetch advances VC,
/// VMLI and MC, and the address is formed from the values before).
#[derive(Clone, Copy, Debug, Default)]
pub struct Phi1Access {
    pub kind: Phi1Kind,
    pub sprite: u8,
    pub addr: u16,
    pub g_vc: u16,
    pub g_char: u8,
    pub g_color: u8,
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
    /// Spec 860 — one entry per line.
    pub lines: Vec<LineSprites>,
    /// Spec 860 — every store that reached the VIC: (clk, register, value).
    pub reg_writes: Vec<(u64, u8, u8)>,
}

impl VicCycleRecorder {
    pub fn armed_at(clk: u64) -> Self {
        VicCycleRecorder {
            clk,
            cycles: Vec::with_capacity(MAX_CYCLES_PER_LINE * MAX_RASTER_LINES + 1024),
            ..Default::default()
        }
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

/// A recorded frame: one frame of the model (19 656 cycles PAL, 17 095 NTSC), the CPU's
/// accesses and instructions over them.
pub struct LineTraceFrame {
    pub which: FrameWhich,
    /// First clk of the frame (its first line, cycle 1).
    pub start_clk: u64,
    /// Spec 863 — the geometry the frame was recorded on.
    pub geometry: FrameGeometry,
    /// The replay drew the same picture the checkpoint shows. `None` when there was
    /// nothing to compare with (the `Next` frame).
    pub verified: Option<bool>,
    /// The anchor the replay started from, and how many cycles it ran before recording.
    pub anchor_clk: u64,
    pub cycles: Vec<VicCycle>,
    pub accesses: Vec<CpuAccess>,
    pub instructions: Vec<Instruction>,
    /// Spec 860 — per line, the sprite registers; and every store that reached the VIC.
    pub sprite_lines: Vec<LineSprites>,
    pub reg_writes: Vec<(u64, u8, u8)>,
}

/// Spec 863 — the frame a record is laid out in: the model's line and frame lengths, the
/// line a displayed frame starts at, and the window the picture is cut from.
#[derive(Clone, Debug)]
pub struct FrameGeometry {
    pub model: &'static str,
    pub chip: &'static str,
    pub cycles_per_line: u64,
    pub lines_per_frame: u64,
    /// The raster line a displayed frame starts at: 0, or on a wrapped window (NTSC) the
    /// line after the window's last — the picture swaps there (VICE's vsync at line 12).
    pub first_line: u64,
    pub window: DisplayWindow,
    pub line: LineGeometry,
}

impl FrameGeometry {
    pub fn of(m: &Machine) -> Self {
        let model = m.model();
        let t = model.timing;
        FrameGeometry {
            model: model.name.as_str(),
            chip: model.vicii.as_str(),
            cycles_per_line: t.cycles_per_line as u64,
            lines_per_frame: t.lines_per_frame as u64,
            first_line: model.window.vsync_line().unwrap_or(0) as u64,
            window: model.window,
            line: m.vic.line_geometry(),
        }
    }
    pub fn cycles_per_frame(&self) -> u64 {
        self.cycles_per_line * self.lines_per_frame
    }
    /// Where a raster line's cycles sit in a record that starts at `first_line`.
    pub fn line_offset(&self, line: u64) -> usize {
        (((line + self.lines_per_frame - self.first_line) % self.lines_per_frame) * self.cycles_per_line) as usize
    }
    /// The canvas y of a raster line (negative, or past the height, outside the window).
    pub fn canvas_y(&self, line: u64) -> i64 {
        self.window.row_of_line(line as u16) as i64 - self.window.first_line as i64
    }
}

/// Where the VIC stands, as a position in its frame: `(line - first line) * cycles per line +
/// (cycle - 1)`, with cycle 1 of line 0 counted to line 0 (the raster counter resets one cycle
/// late). A displayed frame starts at line 0 on PAL, at the window's vsync line on NTSC.
pub fn frame_position(m: &Machine) -> u64 {
    let v = &m.vic;
    let g = FrameGeometry::of(m);
    let line = if v.raster_cycle == 0 && v.start_of_frame { 0 } else { v.raster_line as u64 };
    ((line + g.lines_per_frame - g.first_line) % g.lines_per_frame) * g.cycles_per_line + v.raster_cycle as u64
}

/// The first clk of the next frame (the next displayed-frame boundary).
pub fn next_frame_start(m: &Machine) -> u64 {
    m.c64_core.clk - frame_position(m) + m.timing().cycles_per_frame
}

/// First clk of the frame the machine's `vic.displayed` holds.
///
/// On PAL the picture is published when the raster counter resets (cycle 2 of line 0): from
/// then on `displayed` is the frame that just ended; at cycle 1 of line 0 the reset has not
/// happened, so the picture is still the one before. On a wrapped window (NTSC) it is
/// published at the end of the window's last line — cycle 1 of the vsync line, position 0.
pub fn displayed_frame_start(m: &Machine) -> u64 {
    let g = FrameGeometry::of(m);
    let clk = m.c64_core.clk;
    let pos = frame_position(m);
    let current_start = clk.wrapping_sub(pos);
    let published_at = if g.window.wraps() { 0 } else { 1 };
    let back = if pos >= published_at { g.cycles_per_frame() } else { 2 * g.cycles_per_frame() };
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

    let geometry = FrameGeometry::of(m);
    let mut obs = LineTraceObserver::default();
    m.vic.line_rec = Some(Box::new(VicCycleRecorder::armed_at(m.c64_core.clk)));
    let end = start_clk + geometry.cycles_per_frame();
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
        Some(c)
            if c.line as u64 == geometry.first_line
                && c.cycle == 1
                && cycles.len() as u64 == geometry.cycles_per_frame() => {}
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
    // Compared over the visible window only (384 × 272 on PAL): the draw never writes the
    // framebuffer's last 16 columns, so outside the window both hold whatever they held.
    let w = &geometry.window;
    let verified = reference.map(|r| {
        crate::render::index_buffer_to_canvas_indices(r, w).2
            == crate::render::index_buffer_to_canvas_indices(&m.vic.displayed[..], w).2
    });
    let accesses: Vec<CpuAccess> = obs.accesses.into_iter().filter(|a| a.clk >= start_clk && a.clk < end).collect();
    let instructions: Vec<Instruction> =
        obs.instructions.into_iter().filter(|i| i.end > start_clk && i.start < end).collect();
    let sprite_lines: Vec<LineSprites> =
        rec.lines.into_iter().filter(|l| l.clk >= start_clk && l.clk < end).collect();
    let reg_writes: Vec<(u64, u8, u8)> =
        rec.reg_writes.into_iter().filter(|w| w.0 >= start_clk && w.0 < end).collect();
    Ok(LineTraceFrame {
        which,
        start_clk,
        geometry,
        verified,
        anchor_clk,
        cycles,
        accesses,
        instructions,
        sprite_lines,
        reg_writes,
    })
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
        let g = &self.geometry;
        for line in from..=to.min(g.lines_per_frame as u16 - 1) {
            let base = g.line_offset(line as u64);
            let Some(row) = self.cycles.get(base..base + g.cycles_per_line as usize) else { continue };
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
        let g = &self.geometry;
        json!({
            "which": match self.which { FrameWhich::Displayed => "displayed", FrameWhich::Next => "next" },
            "startClk": self.start_clk,
            "verified": self.verified,
            "anchorClk": self.anchor_clk,
            "replayedCycles": self.start_clk.saturating_sub(self.anchor_clk),
            "model": g.model,
            "chip": g.chip,
            "cyclesPerLine": g.cycles_per_line,
            "linesPerFrame": g.lines_per_frame,
            "firstLine": g.first_line,
            "fbOrigin": { "x": g.window.x0(), "y": g.window.first_line },
            "displayWindow": {
                "firstLine": g.window.first_line,
                "lastLine": g.window.last_line,
                "wraps": g.window.wraps(),
                "width": g.window.width(),
                "height": g.window.height(),
            },
        })
    }
}

// ── Spec 860 — the frame map: the grid, the writes, the objects ─────────────────────────

/// Cell bits of the frame map (one `u16` per line × cycle).
pub mod cell {
    pub const BA: u16 = 1 << 0;
    /// AEC low in Φ2: the VIC owns the bus.
    pub const VIC_OWNS: u16 = 1 << 1;
    /// BA low and no CPU access: the CPU is halted on a read.
    pub const STALL: u16 = 1 << 2;
    pub const C_ACCESS: u16 = 1 << 3;
    pub const S_ACCESS: u16 = 1 << 4;
    pub const P_ACCESS: u16 = 1 << 5;
    pub const G_ACCESS: u16 = 1 << 6;
    pub const REFRESH: u16 = 1 << 7;
    pub const CPU_WRITE: u16 = 1 << 8;
    pub const CPU_READ: u16 = 1 << 9;
    pub const BAD_LINE: u16 = 1 << 10;
    pub const VIC_WRITE: u16 = 1 << 11;
    pub const IDLE_G: u16 = 1 << 12;
}

/// Registers whose value shapes the picture: a store to one inside the visible part of a line
/// changes the picture from that pixel on.
fn shapes_picture(reg: u8) -> bool {
    matches!(reg, 0x00..=0x11 | 0x15..=0x18 | 0x1b..=0x1d | 0x20..=0x2e)
}


/// The display mode of a g-access, the per-cell multicolour bit included.
fn g_mode(c: &VicCycle) -> &'static str {
    let ecm = c.d011 & 0x40 != 0;
    let bmm = c.d011 & 0x20 != 0;
    let mcm = c.d016 & 0x10 != 0;
    match (ecm, bmm, mcm) {
        (false, false, false) => "text",
        (false, false, true) => {
            if c.g_color & 0x08 != 0 {
                "text multicolour"
            } else {
                "text hires (in MC mode)"
            }
        }
        (true, false, false) => "text ECM",
        (false, true, false) => "bitmap",
        (false, true, true) => "bitmap multicolour",
        _ => "invalid mode",
    }
}

/// One cell of the display as the frame drew it: a VC position on a run of lines.
struct Cell {
    col: usize,
    vc: u16,
    l0: u16,
    l1: u16,
    mode: &'static str,
    screen: u16,
    /// Charset base for text modes, bitmap base for bitmap modes.
    data: u16,
    bank: u16,
    rom: bool,
    xscroll: u8,
    chr: u8,
    nonempty: bool,
}

fn find(p: &mut [usize], x: usize) -> usize {
    let mut r = x;
    while p[r] != r {
        r = p[r];
    }
    let mut y = x;
    while p[y] != r {
        let n = p[y];
        p[y] = r;
        y = n;
    }
    r
}

fn coalesce(mut spans: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    spans.sort();
    let mut out: Vec<(u32, u32)> = Vec::new();
    for (a, b) in spans {
        if let Some(last) = out.last_mut() {
            if a <= last.1 + 1 {
                last.1 = last.1.max(b);
                continue;
            }
        }
        out.push((a, b));
    }
    out
}

impl FrameGeometry {
    /// Row index of a line's register sample: 20 (cycle 21, the sixth g-access) on every
    /// family — read from where the table puts the first g-access, not from a literal.
    fn key_index(&self) -> usize {
        self.line.first_g as usize + 4
    }
    /// Row index of the mid-line vertical-border read: 29 (cycle 30), inside the display
    /// fetches after both left-border checks.
    fn vborder_index(&self) -> usize {
        self.line.first_g as usize + 13
    }
    /// First row index of the right-border tail: the last right-border check (ChkBrdR1,
    /// cycle 57) to the end of the line (63 PAL, 65 NTSC).
    fn right_border_from(&self) -> usize {
        self.line.brd_r1 as usize - 1
    }
    fn vis_x0(&self) -> i64 {
        self.window.x0() as i64
    }
    fn vis_w(&self) -> i64 {
        self.window.width() as i64
    }
}

impl LineTraceFrame {
    /// One raster line's cycles.
    fn row(&self, l: usize) -> &[VicCycle] {
        let g = &self.geometry;
        let off = g.line_offset(l as u64);
        &self.cycles[off..off + g.cycles_per_line as usize]
    }

    /// The visible-frame x of the pixels drawn in each cycle (1..=cycles per line), `None`
    /// outside the visible window. The draw column of a cycle is the same on every line.
    fn cycle_x(&self) -> Vec<Option<i64>> {
        let g = &self.geometry;
        let row100 = g.window.row_of_line(100);
        self.row(100)
            .iter()
            .map(|c| {
                let x = c.fb_x as i64 - g.vis_x0();
                (c.fb_line == row100 && (0..g.vis_w()).contains(&x)).then_some(x)
            })
            .collect()
    }

    /// Spec 860 — everything the frozen-frame view draws, from one recorded frame.
    pub fn frame_map_json(&self, include_cells: bool) -> Value {
        let cyc_x = self.cycle_x();
        let geo = &self.geometry;
        let n = geo.cycles_per_line as usize;
        let nl = geo.lines_per_frame as usize;
        let key_i = geo.key_index();

        // CPU accesses per clk.
        let mut acc_at: std::collections::HashMap<u64, (bool, bool)> = std::collections::HashMap::new();
        for a in &self.accesses {
            let e = acc_at.entry(a.clk).or_insert((false, false));
            match a.kind {
                BusKind::Write | BusKind::DummyWrite => e.0 = true,
                _ => e.1 = true,
            }
        }
        let clk_index = |clk: u64| -> Option<(usize, usize)> {
            let i = clk.checked_sub(self.start_clk)? as usize;
            (i < self.cycles.len()).then_some(((i / n + geo.first_line as usize) % nl, i % n))
        };
        let vic_write_at: std::collections::HashSet<u64> = self.reg_writes.iter().map(|w| w.0).collect();

        // ── cells + per-line summary ──
        let mut cells: Vec<Vec<u16>> = Vec::with_capacity(nl);
        let mut lines = Vec::with_capacity(nl);
        for l in 0..nl {
            let row = self.row(l);
            let bad = row.iter().any(|c| c.bad_line);
            let (mut stalled, mut owned, mut ba_n) = (0u32, 0u32, 0u32);
            let mut dma = 0u8;
            let mut out = Vec::with_capacity(n);
            for c in row {
                let (w, r) = acc_at.get(&c.clk).copied().unwrap_or((false, false));
                let mut b = 0u16;
                if c.ba {
                    b |= cell::BA;
                    ba_n += 1;
                }
                if c.aec {
                    b |= cell::VIC_OWNS;
                    owned += 1;
                }
                if c.ba && !w && !r {
                    b |= cell::STALL;
                    stalled += 1;
                }
                if c.phi2 == Phi2Kind::Matrix {
                    b |= cell::C_ACCESS;
                }
                if c.phi2 == Phi2Kind::SpriteData || c.phi1 == Phi1Kind::SpriteData {
                    b |= cell::S_ACCESS;
                }
                match c.phi1 {
                    Phi1Kind::SpritePointer => b |= cell::P_ACCESS,
                    Phi1Kind::Graphics => b |= cell::G_ACCESS,
                    Phi1Kind::IdleGraphics => b |= cell::IDLE_G,
                    Phi1Kind::Refresh => b |= cell::REFRESH,
                    _ => {}
                }
                if w {
                    b |= cell::CPU_WRITE;
                }
                if r {
                    b |= cell::CPU_READ;
                }
                if bad {
                    b |= cell::BAD_LINE;
                }
                if vic_write_at.contains(&c.clk) {
                    b |= cell::VIC_WRITE;
                }
                dma |= c.sprite_dma;
                out.push(b);
            }
            lines.push(json!({ "line": l, "badLine": bad, "ba": ba_n, "stalled": stalled, "vicOwned": owned, "spriteDma": dma }));
            cells.push(out);
        }

        // ── writes that reached the VIC ──
        let writes: Vec<Value> = self
            .reg_writes
            .iter()
            .filter_map(|&(clk, reg, value)| {
                let (l, i) = clk_index(clk)?;
                let x = cyc_x[i];
                let visible_line = geo.window.shows_line(l as u16);
                let mid = shapes_picture(reg) && x.is_some() && visible_line;
                Some(json!({
                    "line": l,
                    "cycle": i + 1,
                    "reg": reg,
                    "addr": 0xd000u16 + reg as u16,
                    "value": value,
                    "x": x,
                    "shapesPicture": shapes_picture(reg),
                    "midLine": mid,
                }))
            })
            .collect();

        // ── display objects: connected non-empty cells sharing mode and source ──
        let mut cells_list: Vec<Cell> = Vec::new();
        // per column: index of the open cell
        let mut open: [Option<usize>; 40] = [None; 40];
        for l in 0..nl {
            let row = self.row(l);
            let mut seen = [false; 40];
            for (j, c) in row.iter().filter(|c| c.phi1 == Phi1Kind::Graphics).enumerate().take(40) {
                seen[j] = true;
                let mode = g_mode(c);
                let bitmap = c.d011 & 0x20 != 0;
                let screen = c.vbank.wrapping_add(((c.d018 >> 4) as u16) * 0x400);
                let data = if bitmap {
                    c.vbank.wrapping_add(((c.d018 & 0x08) as u16) << 10)
                } else {
                    c.vbank.wrapping_add((((c.d018 >> 1) & 0x07) as u16) * 0x800)
                };
                let rom = !bitmap && c.vbank & 0x4000 == 0 && (data & 0x7000) == 0x1000;
                let extend = open[j].and_then(|k| {
                    let e = &cells_list[k];
                    (e.vc == c.g_vc && e.l1 as usize + 1 == l && e.mode == mode && e.screen == screen && e.data == data)
                        .then_some(k)
                });
                match extend {
                    Some(k) => {
                        let e = &mut cells_list[k];
                        e.l1 = l as u16;
                        e.nonempty |= c.phi1_data != 0;
                    }
                    None => {
                        cells_list.push(Cell {
                            col: j,
                            vc: c.g_vc,
                            l0: l as u16,
                            l1: l as u16,
                            mode,
                            screen,
                            data,
                            bank: c.vbank,
                            rom,
                            xscroll: c.d016 & 0x07,
                            chr: c.g_char,
                            nonempty: c.phi1_data != 0,
                        });
                        open[j] = Some(cells_list.len() - 1);
                    }
                }
            }
            for (j, s) in seen.iter().enumerate() {
                if !s {
                    open[j] = None;
                }
            }
        }
        let same_key = |a: &Cell, b: &Cell| a.mode == b.mode && a.screen == b.screen && a.data == b.data && a.bank == b.bank;
        let mut parent: Vec<usize> = (0..cells_list.len()).collect();
        // Index cells by (row start line, column) for the neighbour search.
        let mut by_pos: std::collections::HashMap<(u16, usize), usize> = std::collections::HashMap::new();
        for (k, c) in cells_list.iter().enumerate() {
            by_pos.insert((c.l0, c.col), k);
        }
        for k in 0..cells_list.len() {
            if !cells_list[k].nonempty {
                continue;
            }
            let (l0, l1, col) = (cells_list[k].l0, cells_list[k].l1, cells_list[k].col);
            // Right neighbour, or across one empty cell of the same row.
            for gap in [1usize, 2] {
                if let Some(&m) = by_pos.get(&(l0, col + gap)) {
                    if cells_list[m].nonempty && same_key(&cells_list[k], &cells_list[m]) {
                        let (a, b) = (find(&mut parent, k), find(&mut parent, m));
                        parent[a] = b;
                        break;
                    }
                    if gap == 1 && cells_list[m].nonempty {
                        break; // a different object sits right next to it
                    }
                }
            }
            // The cell directly below.
            if let Some(&m) = by_pos.get(&(l1 + 1, col)) {
                if cells_list[m].nonempty && same_key(&cells_list[k], &cells_list[m]) {
                    let (a, b) = (find(&mut parent, k), find(&mut parent, m));
                    parent[a] = b;
                }
            }
        }
        let mut groups: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
        for (k, c) in cells_list.iter().enumerate() {
            if c.nonempty {
                let r = find(&mut parent, k);
                groups.entry(r).or_default().push(k);
            }
        }
        let mut objects: Vec<Value> = Vec::new();
        for ks in groups.values() {
            let c0 = &cells_list[ks[0]];
            let bitmap = c0.mode.starts_with("bitmap");
            let (mut x0, mut x1, mut y0, mut y1) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
            let mut screen: Vec<(u32, u32)> = Vec::new();
            let mut color: Vec<(u32, u32)> = Vec::new();
            let mut data: Vec<(u32, u32)> = Vec::new();
            let mut chars = std::collections::BTreeSet::new();
            for &k in ks {
                let c = &cells_list[k];
                let x = 32 + 8 * c.col as i64 + c.xscroll as i64;
                x0 = x0.min(x);
                x1 = x1.max(x + 8);
                y0 = y0.min(geo.canvas_y(c.l0 as u64));
                y1 = y1.max(geo.canvas_y(c.l1 as u64) + 1);
                let vc = (c.vc & 0x3ff) as u32;
                screen.push((c.screen as u32 + vc, c.screen as u32 + vc));
                color.push((0xd800 + vc, 0xd800 + vc));
                if bitmap {
                    let a = c.data as u32 + vc * 8;
                    data.push((a, a + 7));
                } else {
                    chars.insert(c.chr);
                    let ch = if c.mode == "text ECM" { c.chr & 0x3f } else { c.chr } as u32;
                    let a = c.data as u32 + ch * 8;
                    data.push((a, a + 7));
                }
            }
            let rng = |v: Vec<(u32, u32)>| -> Vec<Value> {
                coalesce(v).into_iter().map(|(a, b)| json!({ "addr": a, "length": b - a + 1 })).collect()
            };
            let cols = (x1 - x0) / 8;
            let label = format!(
                "{} · {} ${:04x}{} · screen ${:04x} · bank ${:04x} · {} cells",
                c0.mode,
                if bitmap { "bitmap" } else { "chars" },
                c0.data,
                if c0.rom { " (ROM)" } else { "" },
                c0.screen,
                c0.bank,
                ks.len()
            );
            objects.push(json!({
                "kind": "display",
                "mode": c0.mode,
                "label": label,
                "x": x0, "y": y0, "w": x1 - x0, "h": y1 - y0,
                "cells": ks.len(),
                "cols": cols,
                "bank": c0.bank,
                "screenBase": c0.screen,
                "dataBase": c0.data,
                "rom": c0.rom,
                "chars": chars.len(),
                "ranges": {
                    "screen": rng(screen),
                    "colour": rng(color),
                    (if bitmap { "bitmap" } else { "charset" }): rng(data),
                },
            }));
        }

        // ── sprites: one frame per appearance ──
        let sl: std::collections::HashMap<u16, &LineSprites> = self.sprite_lines.iter().map(|l| (l.line, l)).collect();
        for i in 0..8usize {
            let mut run: Option<(u16, u16, u16, u8, bool)> = None; // (first line, last line, x, pointer, xexp)
            let flush = |run: Option<(u16, u16, u16, u8, bool)>, objects: &mut Vec<Value>| {
                if let Some((a, b, x, ptr, xexp)) = run {
                    let s = sl.get(&a).copied();
                    let bank = self.row(a as usize)[0].vbank;
                    let d018 = self.row(a as usize)[key_i].d018;
                    let screen = bank.wrapping_add(((d018 >> 4) as u16) * 0x400);
                    let mc = s.map(|s| s.mc >> i & 1 == 1).unwrap_or(false);
                    let yexp = s.map(|s| s.y_expand >> i & 1 == 1).unwrap_or(false);
                    let color = s.map(|s| s.color[i]).unwrap_or(0);
                    let block = bank as u32 + ptr as u32 * 64;
                    let w = if xexp { 48 } else { 24 };
                    objects.push(json!({
                        "kind": "sprite",
                        "sprite": i,
                        "label": format!("sprite {i}{}{} · ptr ${ptr:02x} → ${block:04x} · x {x}", if mc { " · MC" } else { "" }, if xexp || yexp { " · expanded" } else { "" }),
                        "x": x as i64 - 24 + 32, "y": geo.canvas_y(a as u64), "w": w, "h": (b - a + 1) as i64,
                        "lines": [a, b],
                        "pointer": ptr,
                        "mc": mc, "xExpand": xexp, "yExpand": yexp, "color": color,
                        "ranges": {
                            "sprite": [{ "addr": block, "length": 63 }],
                            "pointer": [{ "addr": screen as u32 + 0x3f8 + i as u32, "length": 1 }],
                        },
                    }));
                }
            };
            // A sprite line is drawn from the bytes the chip fetched for it: the s-accesses of
            // sprites 0–2 fall at the end of the line before (from sprite 0's pointer fetch —
            // cycle 58 PAL, 59 NTSC — on), those of 3–7 at the start of the line itself. The
            // display bit lags a line behind the last fetch, so it would add a 22nd line to a
            // 21-line sprite.
            let next_line_from = geo.line.spr0_ptr;
            let mut drawn = vec![false; nl + 1];
            for c in self.cycles.iter() {
                let is_s = (c.phi1 == Phi1Kind::SpriteData && c.phi1_sprite as usize == i)
                    || (c.phi2 == Phi2Kind::SpriteData && c.phi2_sprite as usize == i);
                if is_s {
                    let l = c.line as usize + usize::from(c.cycle as u16 >= next_line_from);
                    if l < drawn.len() {
                        drawn[l] = true;
                    }
                }
            }
            for (l, &shown) in drawn.iter().enumerate().take(nl) {
                let st = sl.get(&(l as u16)).copied();
                let cur = if shown { st.map(|s| (s.x[i], s.pointer[i], s.x_expand >> i & 1 == 1)) } else { None };
                run = match (run, cur) {
                    (Some((a, b, x, p, e)), Some((cx, cp, ce))) if cx == x && cp == p && ce == e && b as usize + 1 == l => Some((a, l as u16, x, p, e)),
                    (prev, Some((cx, cp, ce))) => {
                        flush(prev, &mut objects);
                        Some((l as u16, l as u16, cx, cp, ce))
                    }
                    (prev, None) => {
                        flush(prev, &mut objects);
                        None
                    }
                };
            }
            flush(run, &mut objects);
        }

        let techniques = self.techniques(&objects, &writes);

        let mut out = json!({
            "frame": self.header_json(),
            "techniques": techniques,
            "geometry": {
                "visible": {
                    "fbX": geo.vis_x0(), "fbY": geo.window.first_line, "w": geo.vis_w(), "h": geo.window.height(),
                    "lastLine": geo.window.last_line, "wraps": geo.window.wraps(),
                },
                "cycleX": cyc_x,
            },
            "cellBits": {
                "ba": cell::BA, "vicOwns": cell::VIC_OWNS, "stall": cell::STALL, "cAccess": cell::C_ACCESS,
                "sAccess": cell::S_ACCESS, "pAccess": cell::P_ACCESS, "gAccess": cell::G_ACCESS,
                "refresh": cell::REFRESH, "cpuWrite": cell::CPU_WRITE, "cpuRead": cell::CPU_READ,
                "badLine": cell::BAD_LINE, "vicWrite": cell::VIC_WRITE, "idleG": cell::IDLE_G,
            },
            "lines": lines,
            "writes": writes,
            "objects": objects,
        });
        if include_cells {
            out["cells"] = json!(cells);
        }
        out
    }
}

// ── Spec 860 — the techniques, as rules over the record ─────────────────────────────────
//
// Each rule is a predicate on what the chip did in this frame — bad lines, idle state,
// border flip-flops, fetch cycles, sprite fetches, register stores — named after the
// technique the demo-coding literature uses for it (Åkesson's VIC timing chart and MISC notes,
// Bauer's VIC article, the vicspector trick reference). A rule fires only on the evidence; it
// says which lines and why. None of them predicts anything.

/// Runs of consecutive line numbers.
fn runs(lines: &[usize]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &l in lines {
        match out.last_mut() {
            Some(r) if r.1 + 1 == l => r.1 = l,
            _ => out.push((l, l)),
        }
    }
    out
}

impl LineTraceFrame {
    fn techniques(&self, objects: &[Value], writes: &[Value]) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        let mut push = |rule: &str, name: &str, a: usize, b: usize, detail: String, source: &str| {
            out.push(json!({ "rule": rule, "name": name, "lines": [a, b], "detail": detail, "source": source }));
        };
        let geo = &self.geometry;
        let nl = geo.lines_per_frame as usize;
        let (key_i, vb_i, rb_from) = (geo.key_index(), geo.vborder_index(), geo.right_border_from());
        let visible = |l: usize| geo.window.shows_line(l as u16);

        // Split: the mode or the memory the VIC reads changes between two lines.
        let key = |r: &[VicCycle]| {
            let c = &r[key_i];
            (c.d011 & 0x60, c.d016 & 0x10, c.d018, c.vbank)
        };
        let mut split_lines = Vec::new();
        let mut split_detail: std::collections::BTreeMap<usize, String> = std::collections::BTreeMap::new();
        for l in 1..nl {
            let (a, b) = (key(self.row(l - 1)), key(self.row(l)));
            if a != b && visible(l) {
                let mut d = Vec::new();
                if a.0 != b.0 || a.1 != b.1 {
                    d.push(format!("mode {}→{}", g_mode(&self.row(l - 1)[key_i]), g_mode(&self.row(l)[key_i])));
                }
                if a.2 != b.2 {
                    d.push(format!("$D018 ${:02x}→${:02x}", a.2, b.2));
                }
                if a.3 != b.3 {
                    d.push(format!("bank ${:04x}→${:04x}", a.3, b.3));
                }
                split_lines.push(l);
                split_detail.insert(l, d.join(", "));
            }
        }
        for (a, b) in runs(&split_lines) {
            if a == b {
                push("split", "Raster split", a, b, format!("at line {a}: {}", split_detail[&a]), "a register change between two lines");
            } else {
                push("split", "Split on every line", a, b, format!("{} lines change mode or memory, first: {}", b - a + 1, split_detail[&a]), "a register change on every line — FLI-style");
            }
        }

        // FLI: a bad line on consecutive lines. A plain screen has one in eight.
        let bad: Vec<usize> = (0..nl).filter(|&l| self.row(l).iter().any(|c| c.bad_line)).collect();
        for (a, b) in runs(&bad) {
            if b - a + 1 >= 4 {
                let d018: std::collections::BTreeSet<u8> = (a..=b).map(|l| self.row(l)[key_i].d018).collect();
                push("fli", "FLI", a, b, format!("{} consecutive bad lines, {} different $D018 values", b - a + 1, d018.len()), "Åkesson, VIC timing chart: a bad line forced on every line");
            }
        }

        // FLD: idle lines inside the display window, between two display rows — the next bad
        // line was pushed down.
        let display = |l: usize| self.row(l).iter().any(|c| c.phi1 == Phi1Kind::Graphics);
        let idle = |l: usize| self.row(l).iter().any(|c| c.phi1 == Phi1Kind::IdleGraphics) && !display(l);
        let first_disp = (0..nl).find(|&l| display(l));
        let last_disp = (0..nl).rev().find(|&l| display(l));
        if let (Some(f), Some(e)) = (first_disp, last_disp) {
            let gaps: Vec<usize> = (f..=e).filter(|&l| idle(l)).collect();
            for (a, b) in runs(&gaps) {
                push("fld", "FLD", a, b, format!("{} idle lines between display rows — the bad line was pushed down", b - a + 1), "vicspector trick reference: FLD");
            }
        }

        // Linecrunch: a display row that ends before its eighth line and the next row starts
        // straight after it.
        let row_start = |l: usize| self.row(l).iter().find(|c| c.phi1 == Phi1Kind::Graphics).map(|c| c.g_vc);
        let mut l = 0;
        while l < nl {
            let Some(vc) = row_start(l) else {
                l += 1;
                continue;
            };
            let mut e = l;
            while e + 1 < nl && row_start(e + 1) == Some(vc) {
                e += 1;
            }
            let len = e - l + 1;
            if len < 8 && e + 1 < nl && row_start(e + 1).is_some() && first_disp != Some(l) {
                push("linecrunch", "Linecrunch", l, e, format!("a character row of {len} lines, the next row starts at line {}", e + 1), "vicspector trick reference: linecrunch");
            }
            l = e + 1;
        }

        // DMA delay (VSP): a bad line whose c-accesses do not start where the table puts the
        // first one (cycle 15).
        let first_c = geo.line.first_c;
        for &l in &bad {
            if let Some(c) = self.row(l).iter().find(|c| c.phi2 == Phi2Kind::Matrix) {
                if c.cycle as u16 != first_c {
                    push("dma_delay", "DMA delay (VSP)", l, l, format!("the c-accesses start at cycle {} instead of {first_c}", c.cycle), "vicspector trick reference: DMA delay");
                }
            }
        }

        // Side borders open: the vertical border is off and the main border flip-flop never
        // closes at the right edge.
        let side: Vec<usize> = (0..nl)
            .filter(|&l| visible(l))
            .filter(|&l| {
                let r = self.row(l);
                !r[vb_i].vertical_border && r[rb_from..].iter().all(|c| !c.main_border)
            })
            .collect();
        for (a, b) in runs(&side) {
            push("side_border", "Side borders open", a, b, format!("{} lines where the main border never closes", b - a + 1), "vicspector trick reference: opening the side borders");
        }

        // Top/bottom border open: lines outside the display window where the vertical border
        // flip-flop is off.
        let tb: Vec<usize> = (0..nl)
            .filter(|&l| visible(l))
            .filter(|&l| {
                let r = self.row(l);
                let rsel = r[key_i].d011 & 0x08 != 0;
                let (top, bottom) = if rsel { (51, 250) } else { (55, 246) };
                (l < top || l > bottom) && !r[vb_i].vertical_border
            })
            .collect();
        for (a, b) in runs(&tb) {
            push("tb_border", "Top/bottom border open", a, b, format!("{} lines outside the display window with the vertical border off", b - a + 1), "Bauer, VIC article: the vertical border flip-flop");
        }

        // Sprites: a multiplexer, and heights that are not 21 (or 42 expanded).
        let mut by_sprite: std::collections::BTreeMap<u64, Vec<&Value>> = std::collections::BTreeMap::new();
        for o in objects.iter().filter(|o| o["kind"] == "sprite") {
            by_sprite.entry(o["sprite"].as_u64().unwrap_or(0)).or_default().push(o);
        }
        for (i, fs) in &by_sprite {
            if fs.len() >= 2 {
                let a = fs[0]["lines"][0].as_u64().unwrap_or(0) as usize;
                let b = fs[fs.len() - 1]["lines"][1].as_u64().unwrap_or(0) as usize;
                push("multiplexer", "Sprite multiplexer", a, b, format!("sprite {i} is drawn {} times in the frame", fs.len()), "one sprite reused down the screen");
            }
            for f in fs {
                let (a, b) = (f["lines"][0].as_u64().unwrap_or(0) as usize, f["lines"][1].as_u64().unwrap_or(0) as usize);
                let h = b - a + 1;
                let want = if f["yExpand"] == true { 42 } else { 21 };
                if h != want && a > 0 && b + 1 < nl {
                    push("sprite_height", if h < want { "Sprite crunch" } else { "Sprite stretch" }, a, b, format!("sprite {i} is {h} lines high, not {want}"), "Åkesson, MISC notes: sprite crunch and stretch");
                }
            }
        }

        // Mid-line stores to a register that shapes the picture.
        let mut mid: std::collections::BTreeMap<u64, Vec<usize>> = std::collections::BTreeMap::new();
        for w in writes.iter().filter(|w| w["midLine"] == true) {
            mid.entry(w["reg"].as_u64().unwrap_or(0)).or_default().push(w["line"].as_u64().unwrap_or(0) as usize);
        }
        for (reg, ls) in mid {
            let (a, b) = (ls[0], ls[ls.len() - 1]);
            push("mid_line", "Mid-line change", a, b, format!("{} store(s) to $D0{reg:02X} inside the visible part of a line", ls.len()), "the change starts at the pixel the store lands on");
        }
        out
    }
}
