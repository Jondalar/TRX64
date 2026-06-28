//! Typed uniffi RECORD structs — one per handler response shape.
//!
//! Each struct mirrors the EXACT `json!({...})` the corresponding `dispatch`
//! handler returns (field names verified against the live handlers). `serde` does
//! the JSON→struct decode in the façade; `uniffi::Record` exposes the struct to
//! Swift (Rust snake_case fields → Swift camelCase properties). The JSON keys are
//! camelCase, so each struct carries `#[serde(rename_all = "camelCase")]` (with a
//! few explicit renames where a key is not a clean camelCase of the field).
//!
//! Optional JSON keys (a handler may omit `stop`, `media`, `prompt`, …) map to
//! `Option<T>` with `#[serde(default)]`, so a missing key decodes as `None` rather
//! than failing the typed decode.

use serde::Deserialize;

// ── session ─────────────────────────────────────────────────────────────────

/// `session/create` result.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub session_id: String,
    pub mode: String,
    pub disk_path: String,
    pub attached: bool,
    pub c64_cycles: u64,
    pub pc: u32,
    /// Present only when the session was created with a trace.
    #[serde(default)]
    pub trace: Option<TraceRun>,
}

/// CPU register file (`session/state` → `cpu`).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuState {
    pub pc: u32,
    pub a: u32,
    pub x: u32,
    pub y: u32,
    pub sp: u32,
    pub flags: u32,
    pub cycles: u64,
}

/// VIC raster + pointer state (`session/state` → `vic`).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VicState {
    pub raster_line: u32,
    pub raster_cycle: u32,
    pub mode: u32,
    pub bank: u32,
    pub screen_ptr: u32,
    pub chargen_ptr: u32,
    pub bitmap_ptr: u32,
    pub border: u32,
    pub background: u32,
}

/// Interrupt/trap flow state (`session/state` → `flow`).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowState {
    pub focus: String,
    pub current: String,
}

/// IRQ/NMI vectors (`session/state` → `vectors`).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Vectors {
    pub irq: u32,
    pub nmi: u32,
    pub cinv: u32,
    pub cbinv: u32,
}

/// SID register snapshot (`session/state` → `sid`).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidState {
    pub regs: Vec<u32>,
    pub streaming: bool,
}

/// Full machine state (`session/state`).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineState {
    pub c64_cycles: u64,
    pub drive_cycles: u64,
    pub mode: String,
    /// "running" | "paused".
    pub run_state: String,
    pub cpu: CpuState,
    pub vic: VicState,
    pub flow: FlowState,
    pub vectors: Vectors,
    pub sid: SidState,
    #[serde(default)]
    pub stop_reason: Option<String>,
}

/// `session/reset` result.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetResult {
    pub c64_cycles: u64,
    pub pc: u32,
    /// "cold" | "soft".
    pub mode: String,
}

// ── run / step / pacing ───────────────────────────────────────────────────────

/// Pacing input (`session/set_pacing`). `mode` ∈ {"pal","warp","fixed-ratio"}.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Pacing {
    pub mode: String,
    pub ratio: f64,
}

/// The pacing sub-object inside a debug-state reply.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PacingState {
    pub mode: String,
    pub ratio: f64,
}

/// A numbered breakpoint (debug-state `breakpoints[]`).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakpointInfo {
    pub num: u32,
    pub addr: u32,
}

/// The stop descriptor inside a debug-state reply.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopInfo {
    pub reason: String,
    pub pc: u32,
    pub cycles: u64,
}

/// `debug/run|pause|continue|step|set_pacing` result (`build_debug_state`).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugState {
    /// "running" | "paused".
    pub run_state: String,
    pub pacing: PacingState,
    pub pc: u32,
    pub cycles: u64,
    pub frame: u64,
    pub breakpoints: Vec<BreakpointInfo>,
    #[serde(default)]
    pub stop: Option<StopInfo>,
    pub control_owner: String,
}

/// A breakpoint hit reported inside `session/run`.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunBreakpoint {
    pub pc: u32,
    pub num: u32,
}

/// `session/run` (run N cycles) result.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub c64_cycles: u64,
    /// Set when the run stopped early on a breakpoint.
    #[serde(default)]
    pub breakpoint: Option<RunBreakpoint>,
}

// ── input ─────────────────────────────────────────────────────────────────────

/// `session/type` result.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeResult {
    pub c64_cycles: u64,
    pub queued: u64,
}

/// Joystick direction/fire state. `is_idle()` (all false) → `joystick_clear`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct JoystickState {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    pub fire: bool,
}

impl JoystickState {
    pub(crate) fn is_idle(&self) -> bool {
        !(self.up || self.down || self.left || self.right || self.fire)
    }
}

/// `session/load_prg` result.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadResult {
    pub load_address: u32,
    pub end_address: u32,
    pub bytes_loaded: u64,
    pub path: String,
}

/// `runtime/run_prg` result.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunPrgResult {
    pub load_address: u32,
    /// The autostart action taken (e.g. "BASIC RUN", "g $0810").
    pub action: String,
}

// ── media ─────────────────────────────────────────────────────────────────────

/// `media/mount` + `media/swap` result. Disk and cart share most fields; the
/// disk-only and cart-only keys are optional.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaResult {
    pub mounted_path: String,
    /// "d64" | "g64" | "crt".
    #[serde(rename = "type")]
    pub kind: String,
    pub sha256: String,
    pub paused: bool,
    /// Disk slot (8) — absent for a cartridge.
    #[serde(default)]
    pub slot: Option<u32>,
    /// Cartridge mapper type — absent for a disk.
    #[serde(default)]
    pub mapper_type: Option<String>,
}

/// `media/unmount` result.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnmountResult {
    pub ok: bool,
    pub paused: bool,
    pub was_running: bool,
}

/// `media/recent` entry.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaEntry {
    pub path: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub mounted_at: Option<String>,
}

/// `session/cart_status` result (null when no cart).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CartStatus {
    #[serde(rename = "type")]
    pub kind: String,
    pub bank: u32,
    /// "write" | "read" | "idle".
    pub activity: String,
    pub booted: bool,
    #[serde(default)]
    pub source_name: Option<String>,
}

// ── trace ─────────────────────────────────────────────────────────────────────

/// Media identity captured at trace start.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceMedia {
    pub sha256: String,
    pub source_name: String,
}

/// `trace/start_domains` → `run` (the active trace run descriptor).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceRun {
    pub run_id: String,
    pub definition_id: String,
    pub definition_version: i64,
    pub cycle_start: u64,
    pub event_count: u64,
    #[serde(default)]
    pub bytes_written: Option<u64>,
    #[serde(default)]
    pub media: Option<TraceMedia>,
}

/// `trace/run/stop` result.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceStatus {
    pub run: TraceRun,
    pub status: String,
    #[serde(default)]
    pub index: Option<IndexResult>,
}

/// `trace/index` result.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexResult {
    pub duckdb_path: String,
    pub events_indexed: u64,
    pub bounded: bool,
    #[serde(default)]
    pub bounded_from: Option<u64>,
    #[serde(default)]
    pub cap: Option<u64>,
    pub indexed_from_oldest: bool,
}

/// `trace/build_from_ring` result.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceFile {
    pub retrace_path: String,
    pub duckdb_path: String,
    pub events_encoded: u64,
}

// ── checkpoint / scrub ────────────────────────────────────────────────────────

/// A checkpoint ref (`checkpoint/capture` → `ref`, `checkpoint/list` → items).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub id: String,
    pub frame: u64,
    pub cycles: u64,
    pub pinned: bool,
}

/// A scrub-filmstrip thumbnail (`checkpoint/thumbnails` → items).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thumbnail {
    pub id: String,
    pub cycles: u64,
    pub frame: u64,
    pub pinned: bool,
    pub width: u32,
    pub height: u32,
    /// Base64 RGB palette.
    pub palette: String,
    /// Base64 colour-index bitmap.
    pub indices: String,
}

// ── live A/V (pull) ───────────────────────────────────────────────────────────

/// The current displayed frame at FULL resolution as a palette + index image
/// (`frameBuffer()`). Same shape as a [`Thumbnail`]'s palette/indices, but full-res
/// (the 384×272 VICE PAL canvas) and NOT base64 — `Vec<u8>` maps to Swift `Data`,
/// and this is an in-process pull (no JSON), so raw bytes are correct + fast.
///
/// To draw: for each of `width*height` pixels, `i = indices[p]` (0..15) selects RGB
/// `palette[i*3 .. i*3+3]`. `palette` is 16×3 = 48 bytes; `indices` is
/// `width*height` bytes. This is NOT a serde record (built directly from core bytes,
/// no JSON decode) — hence no `Deserialize`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FrameBuffer {
    pub width: u32,
    pub height: u32,
    /// RGB palette, 16×3 = 48 bytes (Swift `Data`).
    pub palette: Vec<u8>,
    /// `width*height` palette indices, each 0..15 (Swift `Data`).
    pub indices: Vec<u8>,
}

// ── checkpoint diff (Spec time-travel-tooling Piece 1) ──────────────────────────

/// One contiguous run of changed RAM bytes in a [`SnapshotDiff`]. `old`/`new` are the
/// run's byte payloads (`byteCount` bytes each) BEFORE / AFTER — Swift `Data`.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RamRun {
    /// First changed address of the run.
    pub start: u32,
    /// Number of bytes in the run.
    pub byte_count: u32,
    /// The run's bytes in checkpoint A (base64-decoded to `Data`).
    #[serde(deserialize_with = "de_b64")]
    pub old: Vec<u8>,
    /// The run's bytes in checkpoint B (base64-decoded to `Data`).
    #[serde(deserialize_with = "de_b64")]
    pub new: Vec<u8>,
}

/// One changed register/field in a [`SnapshotDiff`] chip list. `name` is the
/// register's label (CPU: "pc"/"a"/…; chips: "$NN"; CIA: "cia1.$NN"; drive:
/// "via1.$NN"/"headHalfTrack"/…). `old`/`new` are the byte (or 16-bit head) values.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegChange {
    pub name: String,
    pub old: u32,
    pub new: u32,
}

/// `runtime/diff_checkpoints` (= [`Runtime::diff_checkpoints`]) result — a typed,
/// by-ID diff of two checkpoint anchors. RAM is grouped into contiguous changed runs
/// (NOT a 64 K byte list); each chip carries its changed-register list. `cpu`/`vic`/
/// `cia`/`sid`/`drive` are empty when that chip is unchanged (`drive` is empty unless
/// both anchors carried a 1541 DRIVECPU).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDiff {
    pub cycle_a: u64,
    pub cycle_b: u64,
    pub ram: Vec<RamRun>,
    pub cpu: Vec<RegChange>,
    pub vic: Vec<RegChange>,
    pub cia: Vec<RegChange>,
    pub sid: Vec<RegChange>,
    pub drive: Vec<RegChange>,
}

/// serde helper: decode a base64 string JSON value into `Vec<u8>` (the RAM run bytes
/// ride the wire as base64 to stay JSON-safe over `dispatch`).
fn de_b64<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use base64::Engine as _;
    let s: String = String::deserialize(d)?;
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .map_err(serde::de::Error::custom)
}

/// `ringbuffer/dump` + `ringbuffer/restore` (= [`Runtime::ringbuffer_dump`] /
/// [`Runtime::ringbuffer_restore`]) result — a summary of the serialized/loaded
/// reverse-debug buffer (Spec time-travel-tooling Piece 2).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RingDumpInfo {
    pub path: String,
    /// Checkpoint-ring anchor count.
    pub anchors: u64,
    /// Delta-ring (reverse-step / who_wrote) instruction count.
    pub delta_entries: u64,
    /// CPU-history (chis) instruction count.
    pub cpu_history: u64,
    /// Cycle of the oldest anchor (scrub-timeline start).
    pub cycle_first: u64,
    /// Cycle of the newest anchor (scrub-timeline end).
    pub cycle_last: u64,
    /// The "current" anchor id (the scrub head), if any.
    #[serde(default)]
    pub current_id: Option<String>,
    /// On-disk container size in bytes (gzipped).
    pub file_bytes: u64,
    /// Container format version.
    pub version: u32,
}

// ── reverse-debug ─────────────────────────────────────────────────────────────

/// An undone write reported by `runtime/reverse_step`.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoneWrite {
    pub addr: u32,
    pub old: u32,
    pub new: u32,
}

/// `runtime/reverse_step` result (inspect-only).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReverseResult {
    pub steps_taken: u64,
    pub pc: u32,
    pub a: u32,
    pub x: u32,
    pub y: u32,
    pub sp: u32,
    pub p: u32,
    pub cycle: u64,
    pub undone_writes: Vec<UndoneWrite>,
    pub inspect_only: bool,
    pub note: String,
}

/// One writer of an address (`runtime/who_wrote` → `writers[]`).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Writer {
    pub pc: u32,
    pub cycle: u64,
    pub addr: u32,
    pub old: u32,
    pub new: u32,
}

/// `runtime/crash_triage` result — the human-readable cause chain.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriageChain {
    /// Rendered triage lines (the `lines` array of the handler).
    pub lines: Vec<String>,
}

/// `runtime/set_reverse_depth` result (read or set).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReverseDepth {
    pub seconds: u64,
    pub delta_entry_capacity: u64,
    pub delta_write_capacity: u64,
    pub cpu_history_capacity: u64,
    pub estimated_ram_mb: f64,
    pub discarded_history: bool,
    pub note: String,
    #[serde(default)]
    pub warning: Option<String>,
}

// ── snapshot ──────────────────────────────────────────────────────────────────

/// One media slot embedded in a snapshot (`snapshot/dump|undump` → `media[]`).
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMedia {
    pub role: String,
    pub format: String,
    pub source_name: String,
    pub sha256: String,
    pub bytes: u64,
}

/// `snapshot/dump` + `snapshot/undump` result.
#[derive(Debug, Clone, uniffi::Record, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInfo {
    pub path: String,
    pub cycle: u64,
    pub pc: u32,
    pub machine: String,
    pub media: Vec<SnapshotMedia>,
    pub breakpoints: u64,
    /// Present on `dump` (file size), absent on `undump`.
    #[serde(default)]
    pub file_bytes: Option<u64>,
}
