//! Spec 863 — a C64 model is configuration, not code.
//!
//! The rows live in `models.toml` (embedded at build, parsed once). Code holds only the
//! building blocks a row can name — the VIC-II cycle-table families (`vic.rs`, ported
//! 1:1 from VICE with their xpos column), the two colour-latency paths of the draw, the
//! 6526 CIA, reSID's 6581/8580 and the ROM set. A row that names a block TRX64 does not
//! have stays in the list, marked with what is missing, and is refused by name when
//! chosen. Nothing here is ever mapped to "something close".
//!
//! Everything a consumer needs to count in frames or seconds is on [`Timing`], and the
//! machine answers it through `Machine::timing()` — the one source.

use std::sync::OnceLock;

use serde::Deserialize;

/// The model file, embedded at build time.
pub const MODELS_TOML: &str = include_str!("../models.toml");

/// The longest raster line any cycle-table family has (VICE `VICII_DRAW_BUFFER_SIZE` / 8).
pub const MAX_CYCLES_PER_LINE: usize = 65;
/// The most raster lines a row may have — the VIC's per-line arrays and the framebuffer
/// are this tall (the PAL frame).
pub const MAX_RASTER_LINES: usize = 312;

/// The ROM files TRX64 boots (the ROM bundle). A row naming any other file needs a ROM
/// this machine does not carry.
pub const ROM_SET: [&str; 3] = ["kernal-901227-03.bin", "basic-901226-01.bin", "chargen-901225-01.bin"];

/// A VIC-II fetch schedule. Each is a table in `vic.rs`, ported from VICE.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CycleFamily {
    /// `cycle_tab_pal` — 63 cycles (6569, 8565, 6569R1).
    Pal,
    /// `cycle_tab_ntsc` — 65 cycles (6567R8, 8562, 6572).
    Ntsc,
    /// `cycle_tab_ntsc_old` — 64 cycles (6567R56A).
    NtscOld,
}

impl CycleFamily {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pal" => Some(Self::Pal),
            "ntsc" => Some(Self::Ntsc),
            "ntsc-old" => Some(Self::NtscOld),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Pal => "pal",
            Self::Ntsc => "ntsc",
            Self::NtscOld => "ntsc-old",
        }
    }
    /// Cycles per raster line — the length of the family's table.
    pub fn cycles_per_line(self) -> u16 {
        match self {
            Self::Pal => 63,
            Self::Ntsc => 65,
            Self::NtscOld => 64,
        }
    }
}

/// The video standard (VICE `MACHINE_SYNC_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoStandard {
    Pal,
    Ntsc,
    NtscOld,
    PalN,
}

impl VideoStandard {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pal" => Some(Self::Pal),
            "ntsc" => Some(Self::Ntsc),
            "ntsc-old" => Some(Self::NtscOld),
            "pal-n" => Some(Self::PalN),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Pal => "pal",
            Self::Ntsc => "ntsc",
            Self::NtscOld => "ntsc-old",
            Self::PalN => "pal-n",
        }
    }
    /// VICE `MACHINE_SYNC_*` (`machine.h:57-60`).
    pub fn vice_sync(self) -> u32 {
        match self {
            Self::Pal => 1,
            Self::Ntsc => 2,
            Self::NtscOld => 3,
            Self::PalN => 4,
        }
    }
    /// The standard's nominal field rate — the "50 fps" / "60 fps" a UI counts seconds
    /// in. The exact rate is [`Timing::frame_rate`].
    pub fn nominal_fps(self) -> u32 {
        match self {
            Self::Pal | Self::PalN => 50,
            Self::Ntsc | Self::NtscOld => 60,
        }
    }
}

/// The displayed part of the frame (VICE `vicii-timing.h`, "normal" borders).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayWindow {
    /// First displayed raster line.
    pub first_line: u16,
    /// Last displayed raster line. At or past the frame's last line the window wraps:
    /// lines `0..=last_line - raster_lines` are drawn at the bottom (NTSC).
    pub last_line: u16,
    pub border_left: u16,
    pub border_right: u16,
    /// Raster lines per frame (the row's), so the window can map a line to its row.
    pub raster_lines: u16,
}

impl DisplayWindow {
    /// The window runs past the last raster line (NTSC): the frame's first lines are
    /// drawn below its last.
    pub fn wraps(&self) -> bool {
        self.last_line >= self.raster_lines
    }
    /// The canvas: 320 display pixels plus both borders, and every displayed line.
    pub fn width(&self) -> usize {
        (self.border_left + 320 + self.border_right) as usize
    }
    pub fn height(&self) -> usize {
        (self.last_line - self.first_line + 1) as usize
    }
    /// X of the canvas's first pixel in the 520-wide draw buffer (VICE `vicii-draw.c`
    /// `DBUF_OFFSET = 17 * 8 - screen_leftborderwidth`: the draw buffer's column of
    /// display column 0 is 17 cycles in, for every family).
    pub fn x0(&self) -> usize {
        17 * 8 - self.border_left as usize
    }
    /// The framebuffer row a raster line is drawn into. A wrapped window puts the lines
    /// before its first line below the frame (VICE `raster_draw_buffer_ptr_update`), so
    /// the canvas is one contiguous run of rows: `first_line..=last_line`.
    pub fn row_of_line(&self, line: u16) -> u16 {
        if self.wraps() && line < self.first_line {
            line + self.raster_lines
        } else {
            line
        }
    }
    /// The raster line a canvas row shows (`row` counted from the canvas top).
    pub fn line_of_canvas_row(&self, row: usize) -> u16 {
        let r = self.first_line as usize + row;
        if r >= self.raster_lines as usize {
            (r - self.raster_lines as usize) as u16
        } else {
            r as u16
        }
    }
    /// Is this raster line on the canvas?
    pub fn shows_line(&self, line: u16) -> bool {
        let row = self.row_of_line(line);
        row >= self.first_line && row <= self.last_line
    }
    /// The line after which the displayed frame is complete (VICE `vicii.c:440-452`:
    /// vsync at line 0, or on a wrapped window at `last_line - raster_lines + 1`). `None`
    /// is the frame wrap itself.
    pub fn vsync_line(&self) -> Option<u16> {
        self.wraps().then(|| self.last_line - self.raster_lines + 1)
    }
}

/// How long a frame is and how fast the clock runs — the one answer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
    pub cycles_per_line: u16,
    pub lines_per_frame: u16,
    pub cycles_per_frame: u64,
    /// The system clock, Hz.
    pub cpu_hz: u32,
    /// The mains tick the TOD counts, Hz.
    pub tod_hz: u32,
    /// Exact frames per second (`cpu_hz / cycles_per_frame`).
    pub frame_rate: f64,
    /// The standard's nominal rate (50 / 60) — what a UI counts seconds in.
    pub nominal_fps: u32,
    /// VICE `drivesync.c:53-62`: `floor(65536 * 1e6 / cpu_hz)` — the 1541 stays at 1 MHz,
    /// only its ratio to the C64 moves.
    pub drive_sync_factor: u32,
}

impl Timing {
    fn derive(family: CycleFamily, lines: u16, cpu_hz: u32, tod_hz: u32, video: VideoStandard) -> Self {
        let cpl = family.cycles_per_line();
        let cpf = cpl as u64 * lines as u64;
        Timing {
            cycles_per_line: cpl,
            lines_per_frame: lines,
            cycles_per_frame: cpf,
            cpu_hz,
            tod_hz,
            frame_rate: cpu_hz as f64 / cpf as f64,
            nominal_fps: video.nominal_fps(),
            drive_sync_factor: (65536.0 * (1_000_000.0 / cpu_hz as f64)).floor() as u32,
        }
    }
    /// Seconds of emulated time in `cycles`.
    pub fn seconds(&self, cycles: u64) -> f64 {
        cycles as f64 / self.cpu_hz as f64
    }
    /// Emulated cycles in `seconds`.
    pub fn cycles_in(&self, seconds: f64) -> u64 {
        (seconds * self.cpu_hz as f64).floor() as u64
    }
    /// Wall milliseconds of one frame at the nominal rate (20 PAL, 16 NTSC).
    pub fn nominal_frame_ms(&self) -> u64 {
        1000 / self.nominal_fps.max(1) as u64
    }
}

/// One row of `models.toml`, validated.
#[derive(Debug)]
pub struct C64Model {
    pub name: String,
    pub aliases: Vec<String>,
    pub title: String,
    pub video: VideoStandard,
    /// The VIC-II chip, as VICE names it (`6569`, `6567R8`, …).
    pub vicii: String,
    pub cycle_family: CycleFamily,
    pub color_latency: bool,
    pub lightpen_retrigger_x: u8,
    pub lightpen_irq: String,
    pub luminances: String,
    pub window: DisplayWindow,
    pub cia: String,
    pub sid: String,
    pub glue: String,
    pub kernal: String,
    pub basic: String,
    pub chargen: String,
    pub timing: Timing,
    /// The building blocks this row names that TRX64 does not have. Empty = it runs.
    pub missing: Vec<String>,
    /// VICE's VIC-II model number (`vicii.h` `VICII_MODEL_*`) — the byte VICE and the
    /// `.c64re` VIC snapshot record. One row per chip, so it identifies the row.
    pub vicii_id: u8,
}

impl C64Model {
    pub fn runs(&self) -> bool {
        self.missing.is_empty()
    }
    /// Why this row cannot run here, or `None`.
    pub fn refusal(&self) -> Option<String> {
        (!self.missing.is_empty()).then(|| {
            format!("model {} cannot run here: TRX64 has no {}", self.name, self.missing.join(", no "))
        })
    }
    /// reSID's `chip_model` for the row's SID (0 = 6581, 1 = 8580).
    pub fn resid_model(&self) -> i32 {
        if self.sid == "8580" {
            1
        } else {
            0
        }
    }
    pub fn is_default(&self) -> bool {
        std::ptr::eq(self, default_model())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RowToml {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    title: String,
    #[serde(default)]
    default: bool,
    video: String,
    vicii: String,
    cycle_table: String,
    raster_lines: u16,
    cpu_hz: u32,
    tod_hz: u32,
    color_latency: bool,
    lightpen_retrigger_x: u8,
    lightpen_irq: String,
    luminances: String,
    display_window: [u16; 2],
    border: [u16; 2],
    cia: String,
    sid: String,
    glue: String,
    kernal: String,
    basic: String,
    chargen: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileToml {
    model: Vec<RowToml>,
}

/// VICE `vicii.h:59-71` — the VIC-II model numbers.
fn vicii_id(chip: &str) -> Option<u8> {
    Some(match chip {
        "6569" => 0,
        "8565" => 1,
        "6569R1" => 2,
        "6567R8" => 3,
        "8562" => 4,
        "6567R56A" => 5,
        "6572" => 6,
        _ => return None,
    })
}

/// `kernal-901227-0N.bin` is KERNAL revision N (VICE `c64rom.h:37-52`).
fn rom_label(kind: &str, file: &str) -> String {
    if let Some(rev) = file.strip_prefix("kernal-901227-0").and_then(|r| r.strip_suffix(".bin")) {
        return format!("KERNAL rev{rev} ({file})");
    }
    format!("{kind} ROM {file}")
}

struct Registry {
    rows: Vec<C64Model>,
    default: usize,
}

/// Parse and validate a model file. Structural errors (an unknown field, a family that
/// does not exist, two rows claiming one name or one chip, a window the framebuffer
/// cannot hold) fail the whole file. A row that merely needs a block TRX64 lacks is kept,
/// with `missing` saying which.
fn parse(text: &str) -> Result<Registry, String> {
    let file: FileToml = toml::from_str(text).map_err(|e| format!("models.toml: {e}"))?;
    let mut rows: Vec<C64Model> = Vec::with_capacity(file.model.len());
    let mut default = None;
    let mut seen: Vec<String> = Vec::new();
    for (i, r) in file.model.into_iter().enumerate() {
        let at = |what: String| format!("models.toml row {} ({}): {what}", i + 1, r.name);
        for n in std::iter::once(&r.name).chain(r.aliases.iter()) {
            let key = n.to_ascii_lowercase();
            if seen.contains(&key) {
                return Err(at(format!("name `{n}` is claimed twice")));
            }
            seen.push(key);
        }
        let video = VideoStandard::parse(&r.video).ok_or_else(|| at(format!("unknown video standard `{}`", r.video)))?;
        let family =
            CycleFamily::parse(&r.cycle_table).ok_or_else(|| at(format!("unknown cycle table `{}`", r.cycle_table)))?;
        let vid = vicii_id(&r.vicii).ok_or_else(|| at(format!("unknown VIC-II chip `{}`", r.vicii)))?;
        if rows.iter().any(|m| m.vicii_id == vid) {
            return Err(at(format!("a second row for the {} — the chip identifies the row in a snapshot", r.vicii)));
        }
        if r.raster_lines == 0 || r.raster_lines as usize > MAX_RASTER_LINES {
            return Err(at(format!("{} raster lines (1..={MAX_RASTER_LINES})", r.raster_lines)));
        }
        if r.cpu_hz == 0 || r.tod_hz == 0 {
            return Err(at("cpu_hz and tod_hz must be positive".into()));
        }
        let window = DisplayWindow {
            first_line: r.display_window[0],
            last_line: r.display_window[1],
            border_left: r.border[0],
            border_right: r.border[1],
            raster_lines: r.raster_lines,
        };
        if window.first_line > window.last_line
            || window.first_line >= r.raster_lines
            || window.last_line as usize >= MAX_RASTER_LINES
            || (window.wraps() && window.last_line - r.raster_lines >= window.first_line)
        {
            return Err(at(format!("display window {:?} does not fit {} lines", r.display_window, r.raster_lines)));
        }
        if window.border_left as usize > 17 * 8 || window.width() > crate::render::FB_W - window.x0() {
            return Err(at(format!("borders {:?} do not fit the draw buffer", r.border)));
        }

        let mut missing = Vec::new();
        match r.cia.as_str() {
            "6526" => {}
            other => missing.push(format!("{other} CIA")),
        }
        match r.sid.as_str() {
            "6581" | "8580" => {}
            other => missing.push(format!("{other} SID")),
        }
        match r.glue.as_str() {
            "discrete" => {}
            "custom-ic" => missing.push("custom-IC glue logic".into()),
            other => missing.push(format!("{other} glue logic")),
        }
        if r.lightpen_irq != "new" {
            missing.push(format!("{} light-pen IRQ mode", r.vicii));
        }
        if r.luminances != "new" {
            missing.push(format!("{} luminances", r.vicii));
        }
        for (kind, f) in [("KERNAL", &r.kernal), ("BASIC", &r.basic), ("CHARGEN", &r.chargen)] {
            if !ROM_SET.contains(&f.as_str()) {
                missing.push(format!("{} — not in the ROM set", rom_label(kind, f)));
            }
        }

        if r.default {
            if default.is_some() {
                return Err(at("a second default row".into()));
            }
            if !missing.is_empty() {
                return Err(at("the default row must run".into()));
            }
            default = Some(rows.len());
        }
        rows.push(C64Model {
            timing: Timing::derive(family, r.raster_lines, r.cpu_hz, r.tod_hz, video),
            name: r.name,
            aliases: r.aliases,
            title: r.title,
            video,
            vicii: r.vicii,
            cycle_family: family,
            color_latency: r.color_latency,
            lightpen_retrigger_x: r.lightpen_retrigger_x,
            lightpen_irq: r.lightpen_irq,
            luminances: r.luminances,
            window,
            cia: r.cia,
            sid: r.sid,
            glue: r.glue,
            kernal: r.kernal,
            basic: r.basic,
            chargen: r.chargen,
            missing,
            vicii_id: vid,
        });
    }
    let default = default.ok_or_else(|| "models.toml: no row is `default = true`".to_string())?;
    Ok(Registry { rows, default })
}

fn registry() -> &'static Result<Registry, String> {
    static REG: OnceLock<Result<Registry, String>> = OnceLock::new();
    REG.get_or_init(|| parse(MODELS_TOML))
}

/// Parse the embedded model file, reporting what is wrong with it. A host calls this at
/// startup so a broken file is a clean exit, not a panic later. (The unit tests parse the
/// same file, so a build that ships one that fails does not pass them.)
pub fn check() -> Result<(), String> {
    registry().as_ref().map(|_| ()).map_err(|e| e.clone())
}

fn reg() -> &'static Registry {
    match registry() {
        Ok(r) => r,
        // Unreachable in a tested build — see `check`.
        Err(e) => panic!("{e}"),
    }
}

/// Every row, in file order.
pub fn models() -> &'static [C64Model] {
    &reg().rows
}

/// The machine a fresh session is (`c64-pal`).
pub fn default_model() -> &'static C64Model {
    let r = reg();
    &r.rows[r.default]
}

/// A row by name or alias (case-insensitive), runnable or not.
pub fn find(name: &str) -> Option<&'static C64Model> {
    let n = name.trim().to_ascii_lowercase();
    models().iter().find(|m| m.name == n || m.aliases.iter().any(|a| a.to_ascii_lowercase() == n))
}

/// A row that can run, or why not — the one door every "choose a model" goes through.
pub fn resolve(name: &str) -> Result<&'static C64Model, String> {
    let m = find(name).ok_or_else(|| {
        let known: Vec<&str> = models().iter().map(|m| m.name.as_str()).collect();
        format!("unknown model `{}` (known: {})", name.trim(), known.join(", "))
    })?;
    match m.refusal() {
        Some(why) => Err(why),
        None => Ok(m),
    }
}

/// The row whose VIC-II is `id` (VICE `VICII_MODEL_*`), runnable or not.
pub fn by_vicii_id(id: u8) -> Option<&'static C64Model> {
    models().iter().find(|m| m.vicii_id == id)
}

/// The row for a snapshot's VIC model byte, or why it cannot be restored here.
pub fn for_snapshot(id: u8) -> Result<&'static C64Model, String> {
    let m = by_vicii_id(id).ok_or_else(|| format!("the snapshot's VIC-II model {id} is not a known C64 model"))?;
    match m.refusal() {
        Some(why) => Err(why),
        None => Ok(m),
    }
}

/// One row as the wire shows it: `session/models` lists these.
pub fn row_json(m: &C64Model) -> serde_json::Value {
    let t = &m.timing;
    serde_json::json!({
        "name": m.name,
        "aliases": m.aliases,
        "title": m.title,
        "default": m.is_default(),
        "runs": m.runs(),
        "missing": m.missing,
        "videoStandard": m.video.name(),
        "chip": m.vicii,
        "cycleTable": m.cycle_family.name(),
        "cyclesPerLine": t.cycles_per_line,
        "linesPerFrame": t.lines_per_frame,
        "cyclesPerFrame": t.cycles_per_frame,
        "cpuHz": t.cpu_hz,
        "todHz": t.tod_hz,
        "frameRate": t.frame_rate,
        "cia": m.cia,
        "sid": m.sid,
        "glue": m.glue,
        "kernal": m.kernal,
        "displayWindow": { "firstLine": m.window.first_line, "lastLine": m.window.last_line,
                           "width": m.window.width(), "height": m.window.height() },
    })
}

/// The machine a state reply describes (`session/state`, `monitor/state`, the A/V hello):
/// `model`, `videoStandard`, `chip`, `cyclesPerLine`, `linesPerFrame`, `cyclesPerFrame`,
/// `cpuHz`, `frameRate`, and the canvas it streams.
pub fn identity_json(m: &C64Model) -> serde_json::Map<String, serde_json::Value> {
    let t = &m.timing;
    let mut o = serde_json::Map::new();
    o.insert("model".into(), m.name.clone().into());
    o.insert("videoStandard".into(), m.video.name().into());
    o.insert("chip".into(), m.vicii.clone().into());
    o.insert("cyclesPerLine".into(), t.cycles_per_line.into());
    o.insert("linesPerFrame".into(), t.lines_per_frame.into());
    o.insert("cyclesPerFrame".into(), t.cycles_per_frame.into());
    o.insert("cpuHz".into(), t.cpu_hz.into());
    o.insert("frameRate".into(), serde_json::json!(t.frame_rate));
    o.insert("canvas".into(), serde_json::json!({ "width": m.window.width(), "height": m.window.height() }));
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_toml_parses_and_every_row_is_checked() {
        check().expect("models.toml");
        assert_eq!(models().len(), 7, "VICE's seven C64 rows");
        assert_eq!(default_model().name, "c64-pal");
    }

    #[test]
    fn the_three_rows_that_run_and_why_the_others_do_not() {
        let runs: Vec<&str> = models().iter().filter(|m| m.runs()).map(|m| m.name.as_str()).collect();
        assert_eq!(runs, ["c64-pal", "c64-ntsc", "c64-paln"]);
        let c64c = find("c64c-pal").unwrap();
        assert!(c64c.missing.iter().any(|b| b == "6526A CIA"), "{:?}", c64c.missing);
        assert!(c64c.missing.iter().any(|b| b == "custom-IC glue logic"));
        let err = resolve("c64c-pal").unwrap_err();
        assert!(err.contains("6526A"), "{err}");
        assert!(find("c64-old-pal").unwrap().missing.iter().any(|b| b.starts_with("KERNAL rev2")));
        assert!(find("c64-old-ntsc").unwrap().missing.iter().any(|b| b.starts_with("KERNAL rev1")));
        assert!(resolve("c128").unwrap_err().contains("unknown model"));
    }

    /// The numbers VICE's `c64.h`, `drivesync.c` and `vicii-chip-model.c` give, derived —
    /// never written in a row.
    #[test]
    fn derived_timing_matches_vice() {
        let pal = resolve("c64-pal").unwrap().timing;
        assert_eq!((pal.cycles_per_line, pal.lines_per_frame, pal.cycles_per_frame), (63, 312, 19656));
        assert_eq!((pal.cpu_hz, pal.tod_hz, pal.drive_sync_factor, pal.nominal_fps), (985_248, 50, 66517, 50));
        let ntsc = resolve("ntsc").unwrap().timing;
        assert_eq!((ntsc.cycles_per_line, ntsc.lines_per_frame, ntsc.cycles_per_frame), (65, 263, 17095));
        assert_eq!((ntsc.cpu_hz, ntsc.tod_hz, ntsc.drive_sync_factor, ntsc.nominal_fps), (1_022_730, 60, 64079, 60));
        assert!((ntsc.frame_rate - 59.826).abs() < 0.001, "{}", ntsc.frame_rate);
        let paln = resolve("drean").unwrap().timing;
        assert_eq!((paln.cycles_per_line, paln.lines_per_frame, paln.cycles_per_frame), (65, 312, 20280));
        assert_eq!((paln.cpu_hz, paln.tod_hz), (1_023_440, 50));
        let old = find("c64-old-ntsc").unwrap().timing;
        assert_eq!((old.cycles_per_line, old.lines_per_frame, old.cycles_per_frame), (64, 262, 16768));
    }

    #[test]
    fn the_ntsc_window_wraps_and_the_canvas_is_contiguous() {
        let w = resolve("c64-ntsc").unwrap().window;
        assert!(w.wraps());
        assert_eq!((w.width(), w.height(), w.x0()), (384, 247, 104));
        assert_eq!(w.vsync_line(), Some(12), "VICE vicii.c:449 — vsync at line 12");
        assert_eq!(w.row_of_line(0), 263);
        assert_eq!(w.row_of_line(11), 274);
        assert_eq!(w.row_of_line(28), 28);
        assert_eq!(w.line_of_canvas_row(0), 28);
        assert_eq!(w.line_of_canvas_row(246), 11);
        assert!(w.shows_line(5) && w.shows_line(262) && !w.shows_line(12) && !w.shows_line(27));
        let p = resolve("c64-pal").unwrap().window;
        assert!(!p.wraps());
        assert_eq!((p.width(), p.height(), p.x0(), p.first_line), (384, 272, 104, 16));
        assert_eq!(p.vsync_line(), None);
    }

    #[test]
    fn a_broken_file_is_an_error_not_a_panic() {
        assert!(parse("[[model]]\nname = \"x\"").is_err());
        let two = MODELS_TOML.replace("vicii = \"6567R8\"", "vicii = \"6569\"");
        assert!(matches!(parse(&two), Err(e) if e.contains("second row")));
    }
}
