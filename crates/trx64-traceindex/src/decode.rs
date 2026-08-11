//! `.c64retrace` streaming reader — the native port of the TS reference decoder
//! (`binary-format.ts` `decodeFileHeader` / `decodeEvent`) plus the windowed
//! reader loop from `binary-log-indexer.ts`.
//!
//! **No format change.** Every offset, width and conditional here is taken from
//! the byte-exact spec (Spec 802 R1) and cross-checked against the writer,
//! `trx64_trace::FrameSink`, whose opcode constants this module re-uses so the
//! two cannot drift.
//!
//! Layout recap (all little-endian, packed, no padding):
//! ```text
//! FILE       := FileHeader Event*
//! FileHeader := magic[8]="C64RETR1" version:u16 flags:u16 meta_len:u32 meta_json[meta_len]
//! Event      := opcode:u8 cycle:f64 payload…      (MARK has no leading-cycle exception:
//!                                                  it too is opcode + f64 cycle + u16 len + label)
//! ```
//!
//! Two properties are load-bearing and easy to get wrong:
//!
//! 1. **`cycle` is an f64, not a u64 bit pattern.** The writer stores
//!    `(clk as u64) as f64`. Read it with `f64::from_le_bytes` and truncate
//!    toward zero (`trunc`) to get the integer clock — matching the reference
//!    indexer's `BigInt(Math.trunc(row.cycle))`.
//! 2. **The bounds gate runs BEFORE any field read**, including `cycle`. A
//!    truncated final record (killed trace) or a record straddling the read
//!    window must return [`Step::Incomplete`], never panic on a slice.

use crate::error::{Result, TraceReadError};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

// The format constants come from the WRITER crate — one definition, no drift.
pub use trx64_trace::{ACCESS_READ, ACCESS_WRITE, MAGIC};
/// Highest format version this build can lay out (writer emits 2).
pub const FORMAT_VERSION: u16 = trx64_trace::FORMAT_VERSION;
/// Bytes of the fixed header prefix: magic(8) + version(2) + flags(2) + meta_len(4).
pub const FILE_HEADER_FIXED: usize = 16;

// ── Opcodes ──────────────────────────────────────────────────────────────────
// Mirrors `trx64_trace::TraceOp` + the three RESERVED codes that enum omits
// (0x21 CIA_EVENT, 0x32 VIA_REG_WRITE, 0x33 GCR_EVENT) and 0x22 SID_REG_WRITE /
// 0x40 MEDIA_WRITE. A reader must recognise every opcode the FORMAT defines,
// not just the ones TRX64 emits — TS-written traces are in the corpus.
pub const OP_MARK: u8 = 0x01;
pub const OP_CPU_STEP: u8 = 0x10;
pub const OP_RAM_WRITE: u8 = 0x11;
pub const OP_IO_WRITE: u8 = 0x12;
pub const OP_VIC_REG_WRITE: u8 = 0x20;
pub const OP_CIA_EVENT: u8 = 0x21;
pub const OP_SID_REG_WRITE: u8 = 0x22;
pub const OP_IEC_LINE_CHANGE: u8 = 0x23;
pub const OP_DRIVE_CPU_STEP: u8 = 0x30;
pub const OP_DRIVE_RAM_WRITE: u8 = 0x31;
pub const OP_VIA_REG_WRITE: u8 = 0x32;
pub const OP_GCR_EVENT: u8 = 0x33;
pub const OP_DRIVE_HEAD: u8 = 0x34;
pub const OP_BLOCK_READ: u8 = 0x35;
pub const OP_CART_READ: u8 = 0x36;
pub const OP_MEDIA_WRITE: u8 = 0x40;

/// IEC line bit assignment (`IEC_BIT`, binary-format.ts:384). 9 meaningful bits.
pub const IEC_BIT_ATN: u16 = 1 << 0;
pub const IEC_BIT_CLK: u16 = 1 << 1;
pub const IEC_BIT_DATA: u16 = 1 << 2;
pub const IEC_BIT_C64_ATN: u16 = 1 << 3;
pub const IEC_BIT_C64_CLK: u16 = 1 << 4;
pub const IEC_BIT_C64_DATA: u16 = 1 << 5;
pub const IEC_BIT_DRV_CLK: u16 = 1 << 6;
pub const IEC_BIT_DRV_DATA: u16 = 1 << 7;
pub const IEC_BIT_DRV_ATN_ACK: u16 = 1 << 8;

/// Payload bytes AFTER the opcode, at the given format version.
/// `None` = variable (MARK / MEDIA_WRITE) or unknown ⇒ not skippable by table.
///
/// v1 vs v2: the three mem-access opcodes are ONE BYTE SHORTER at v1 (no
/// trailing `old_value`). This is the only width difference between the two
/// versions and the reason the decoder must be version-parameterised — a v1
/// file decoded as v2 misaligns catastrophically after the first mem record.
#[inline]
pub fn payload_size(op: u8, version: u16) -> Option<usize> {
    let base = match op {
        OP_CPU_STEP | OP_DRIVE_CPU_STEP => 18,
        OP_RAM_WRITE | OP_IO_WRITE | OP_DRIVE_RAM_WRITE => 15,
        OP_VIC_REG_WRITE | OP_CIA_EVENT | OP_VIA_REG_WRITE | OP_GCR_EVENT => 12,
        OP_SID_REG_WRITE => 11,
        OP_IEC_LINE_CHANGE => 10,
        OP_DRIVE_HEAD => 10,
        OP_BLOCK_READ => 12,
        // Spec 785 C1: cycle(8) + bank(2) + slot(1) + off_lo(2) + off_hi(2) + bytes(4).
        OP_CART_READ => 19,
        // 0x01 MARK is handled separately (u16-prefixed label);
        // 0x40 MEDIA_WRITE is variable with NO length prefix ⇒ unskippable.
        _ => return None,
    };
    let is_mem = matches!(op, OP_RAM_WRITE | OP_IO_WRITE | OP_DRIVE_RAM_WRITE);
    Some(if version < 2 && is_mem { base - 1 } else { base })
}

// ── Header ───────────────────────────────────────────────────────────────────

/// The `meta_json` object in the file header.
///
/// `media_sha` / `media_name` / `start_checkpoint_id` are OPTIONAL — TRX64 never
/// writes them. Unknown extra keys are ignored (forward-compat), and `def_json`
/// is deliberately typed as a plain `String`: it is a JSON document
/// double-encoded into a string, and one daemon path writes it EMPTY (see
/// [`ParsedHeader::definition`]).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct TraceFileMeta {
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub def_id: String,
    #[serde(default)]
    pub def_version: f64,
    #[serde(default)]
    pub def_name: String,
    #[serde(default)]
    pub def_json: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub cycle_start: f64,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub media_sha: Option<String>,
    #[serde(default)]
    pub media_name: Option<String>,
    #[serde(default)]
    pub start_checkpoint_id: Option<String>,
}

// The on-disk keys are camelCase (`runId`, `defJson`, `cycleStart`, …) because
// the format was defined by the TS writer. Deserialize with an explicit rename
// rule rather than 11 attributes.
impl TraceFileMeta {
    fn from_json(raw: &str) -> Result<Self> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            #[serde(default)]
            run_id: String,
            #[serde(default)]
            def_id: String,
            #[serde(default)]
            def_version: f64,
            #[serde(default)]
            def_name: String,
            #[serde(default)]
            def_json: String,
            #[serde(default)]
            domains: Vec<String>,
            #[serde(default)]
            cycle_start: f64,
            #[serde(default)]
            created_at: String,
            #[serde(default)]
            media_sha: Option<String>,
            #[serde(default)]
            media_name: Option<String>,
            #[serde(default)]
            start_checkpoint_id: Option<String>,
        }
        let w: Wire = serde_json::from_str(raw)
            .map_err(|e| TraceReadError::BadMeta(e.to_string()))?;
        Ok(TraceFileMeta {
            run_id: w.run_id,
            def_id: w.def_id,
            def_version: w.def_version,
            def_name: w.def_name,
            def_json: w.def_json,
            domains: w.domains,
            cycle_start: w.cycle_start,
            created_at: w.created_at,
            media_sha: w.media_sha.filter(|s| !s.is_empty()),
            media_name: w.media_name.filter(|s| !s.is_empty()),
            start_checkpoint_id: w.start_checkpoint_id.filter(|s| !s.is_empty()),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ParsedHeader {
    pub meta: TraceFileMeta,
    pub version: u16,
    /// Absolute file offset of the first event = `16 + meta_len`.
    pub header_len: usize,
}

impl ParsedHeader {
    /// The trace DEFINITION, parsed out of `meta.def_json`.
    ///
    /// **Robustness divergence from the TS reference (Spec 802 R1 §7.1 / R2 J-5).**
    /// The daemon's `session/create … trace_out` path writes `"defJson": ""`.
    /// The TS indexer does an unguarded `JSON.parse` and therefore throws
    /// `Unexpected end of JSON input` — such a store is un-indexable today.
    /// We parse leniently: empty/invalid ⇒ a synthesized default definition
    /// built from the header's own `defId` / `defName` / `domains`, and the
    /// index build continues. Strictly more traces are readable; the only
    /// divergence is where TS hard-fails.
    ///
    /// Set `TRX64_INDEX_STRICT_DEFJSON=1` to reproduce the TS failure exactly
    /// (needed only to run the byte-parity gate against the sidecar).
    pub fn definition(&self) -> Result<TraceDefinition> {
        TraceDefinition::parse(&self.meta, strict_def_json())
    }
}

fn strict_def_json() -> bool {
    matches!(std::env::var("TRX64_INDEX_STRICT_DEFJSON").as_deref(), Ok("1") | Ok("true"))
}

/// The bits of the trace definition the index writer needs (`name`,
/// `retention`) plus the raw JSON text stored verbatim in `trace_run.def_json`.
///
/// **`raw` is stored verbatim, NOT re-serialized** (Spec 802 R2 J-2). The TS
/// writer stores `JSON.stringify(JSON.parse(defJson))`, which is an identity
/// transform for the compact JSON both producers emit; re-serializing through
/// `serde_json::Value` would alphabetically reorder keys (BTreeMap) and break
/// string parity, and enabling `preserve_order` would perturb every other JSON
/// response in the workspace.
#[derive(Debug, Clone, Default)]
pub struct TraceDefinition {
    pub raw: String,
    pub name: String,
    pub retention: String,
    /// True when `def_json` was empty/invalid and this definition was synthesized.
    pub synthesized: bool,
}

impl TraceDefinition {
    fn parse(meta: &TraceFileMeta, strict: bool) -> Result<Self> {
        let parsed: std::result::Result<serde_json::Value, _> =
            serde_json::from_str(&meta.def_json);
        match parsed {
            Ok(v) => Ok(TraceDefinition {
                name: v
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or(&meta.def_name)
                    .to_string(),
                retention: v
                    .get("retention")
                    .and_then(|x| x.as_str())
                    .unwrap_or("transient")
                    .to_string(),
                raw: meta.def_json.clone(),
                synthesized: false,
            }),
            Err(e) => {
                if strict {
                    return Err(TraceReadError::BadMeta(format!(
                        "defJson: {e} (TRX64_INDEX_STRICT_DEFJSON=1)"
                    )));
                }
                // Synthesize a minimal, VALID definition document from the
                // header fields we do have, so the store still gets a parseable
                // `trace_run.def_json` instead of an empty string.
                let raw = serde_json::to_string(&serde_json::json!({
                    "id": meta.def_id,
                    "version": meta.def_version as i64,
                    "name": if meta.def_name.is_empty() { meta.def_id.clone() } else { meta.def_name.clone() },
                    "domains": meta.domains,
                    "triggers": [],
                    "captures": [],
                    "retention": "transient",
                }))
                .unwrap_or_else(|_| "{}".to_string());
                Ok(TraceDefinition {
                    name: if meta.def_name.is_empty() {
                        meta.def_id.clone()
                    } else {
                        meta.def_name.clone()
                    },
                    retention: "transient".to_string(),
                    raw,
                    synthesized: true,
                })
            }
        }
    }
}

/// Parse the file header out of the leading bytes of a `.c64retrace`.
///
/// `buf` must hold at least `16 + meta_len` bytes; callers stream a bounded
/// window (see [`INDEX_HEADER_MAX`]) rather than slurping a multi-GB file.
pub fn decode_file_header(buf: &[u8]) -> Result<ParsedHeader> {
    if buf.len() < FILE_HEADER_FIXED {
        return Err(TraceReadError::TruncatedHeader);
    }
    if buf[0..8] != MAGIC[..] {
        return Err(TraceReadError::BadMagic);
    }
    let version = u16::from_le_bytes([buf[8], buf[9]]);
    // buf[10..12] = flags — always written 0, NEVER read (reserved). Do not
    // validate it and do not branch on it.
    if !(1..=FORMAT_VERSION).contains(&version) {
        return Err(TraceReadError::UnsupportedVersion(version));
    }
    let meta_len = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]) as usize;
    let meta_end = FILE_HEADER_FIXED
        .checked_add(meta_len)
        .ok_or(TraceReadError::TruncatedMeta)?;
    if buf.len() < meta_end {
        return Err(TraceReadError::TruncatedMeta);
    }
    // Lossy: never fail a whole trace on a stray byte in the header JSON's text.
    let raw = String::from_utf8_lossy(&buf[FILE_HEADER_FIXED..meta_end]);
    let meta = TraceFileMeta::from_json(&raw)?;
    Ok(ParsedHeader { meta, version, header_len: meta_end })
}

// ── Events ───────────────────────────────────────────────────────────────────

/// One decoded record. Field presence is opcode-dependent; the reserved
/// skip-only opcodes (0x21 / 0x32 / 0x33) decode to `op` + `cycle` only, exactly
/// like the reference decoder.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecodedEvent {
    pub op: u8,
    /// The EMITTING side's clock: C64 master clock for 0x01/0x10/0x11/0x12/
    /// 0x20/0x22/0x23/0x36, the DRIVE clock for 0x30/0x31/0x34/0x35. The two lanes
    /// are different time bases — the stream is NOT globally cycle-monotonic.
    /// (0x36 CART_READ is C64-side: the cartridge is a bus hook on the main CPU,
    /// not a clocked device with a domain of its own.)
    pub cycle: f64,
    pub pc: Option<u16>,
    pub opcode: Option<u8>,
    pub a: Option<u8>,
    pub x: Option<u8>,
    pub y: Option<u8>,
    pub sp: Option<u8>,
    pub p: Option<u8>,
    pub b1: Option<u8>,
    pub b2: Option<u8>,
    pub addr: Option<u16>,
    pub value: Option<u8>,
    pub access: Option<u8>,
    /// Present only when the record carries a meaningful pre-write byte
    /// (v2 AND the access byte's bit-7 `has_old` is set).
    pub old_value: Option<u8>,
    pub lines: Option<u16>,
    pub raster_y: Option<u16>,
    pub kind_code: Option<u8>,
    pub reg: Option<u16>,
    pub halftrack: Option<u8>,
    pub sector: Option<u8>,
    /// Bytes read off the block (0x35) / served out of the bank residency (0x36).
    /// 32-bit for the cart lane's sake; the disk lane widens its u16 into it.
    pub bytes: Option<u32>,
    /// Spec 785 C1 (0x36 only): the bank, the window (0 = ROML, 1 = ROMH) and the
    /// inclusive range of window offsets the residency touched.
    pub bank: Option<u16>,
    pub slot: Option<u8>,
    pub off_lo: Option<u16>,
    pub off_hi: Option<u16>,
    pub label: Option<String>,
}

impl DecodedEvent {
    /// The cycle as an integer clock — truncated toward zero, matching the
    /// reference indexer's `BigInt(Math.trunc(row.cycle))`.
    #[inline]
    pub fn clock(&self) -> u64 {
        let t = self.cycle.trunc();
        if t <= 0.0 {
            0
        } else {
            t as u64
        }
    }
}

/// Result of one [`decode_event`] step.
#[derive(Debug)]
pub enum Step {
    Event { ev: DecodedEvent, next: usize },
    /// The full record does not fit in `buf`: either it straddles the read
    /// window (carry the tail and read more) or it is a truncated final record
    /// (at EOF ⇒ drop it silently, that is NOT an error).
    Incomplete,
    /// `off` is at or past the end of `buf`.
    Eof,
}

#[inline]
fn u16le(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

/// Decode ONE record at `off`.
///
/// The bounds gate runs before any field read (including `cycle`) — that is
/// exactly the spot where a naive port panics on a truncated trace.
pub fn decode_event(buf: &[u8], off: usize, version: u16) -> Result<Step> {
    if off >= buf.len() {
        return Ok(Step::Eof);
    }
    let op = buf[off];

    // ── bounds gate ─────────────────────────────────────────────────────────
    let w = if op == OP_MARK {
        // opcode(1) + cycle f64(8) + len u16(2) must be readable first.
        if off + 11 > buf.len() {
            return Ok(Step::Incomplete);
        }
        10 + u16le(buf, off + 9) as usize
    } else {
        match payload_size(op, version) {
            Some(w) => w,
            // 0x40 MEDIA_WRITE and any unknown opcode: variable / no table
            // entry ⇒ the stream cannot be skipped or resynchronised.
            None => return Err(TraceReadError::UnskippableOpcode { op, off }),
        }
    };
    if off + 1 + w > buf.len() {
        return Ok(Step::Incomplete);
    }

    // ── payload ─────────────────────────────────────────────────────────────
    let mut o = off + 1;
    let cycle = f64::from_le_bytes([
        buf[o], buf[o + 1], buf[o + 2], buf[o + 3], buf[o + 4], buf[o + 5], buf[o + 6], buf[o + 7],
    ]);
    o += 8;
    let next = off + 1 + w;
    let mut ev = DecodedEvent { op, cycle, ..Default::default() };

    match op {
        OP_CPU_STEP | OP_DRIVE_CPU_STEP => {
            ev.pc = Some(u16le(buf, o));
            o += 2;
            ev.opcode = Some(buf[o]);
            ev.a = Some(buf[o + 1]);
            ev.x = Some(buf[o + 2]);
            ev.y = Some(buf[o + 3]);
            ev.sp = Some(buf[o + 4]);
            ev.p = Some(buf[o + 5]);
            ev.b1 = Some(buf[o + 6]);
            ev.b2 = Some(buf[o + 7]);
        }
        OP_RAM_WRITE | OP_IO_WRITE | OP_DRIVE_RAM_WRITE => {
            ev.addr = Some(u16le(buf, o));
            o += 2;
            ev.value = Some(buf[o]);
            o += 1;
            ev.pc = Some(u16le(buf, o));
            o += 2;
            let acc = buf[o];
            if version < 2 {
                // v1: the whole byte is the access code; there is no has_old
                // bit and no trailing old_value byte (15-byte record).
                ev.access = Some(acc);
            } else {
                ev.access = Some(acc & 0x7f);
                ev.old_value = if acc & 0x80 != 0 { Some(buf[o + 1]) } else { None };
            }
        }
        OP_IEC_LINE_CHANGE => ev.lines = Some(u16le(buf, o)),
        OP_VIC_REG_WRITE => {
            ev.raster_y = Some(u16le(buf, o));
            ev.kind_code = Some(buf[o + 2]);
            ev.value = Some(buf[o + 3]);
        }
        OP_SID_REG_WRITE => {
            ev.reg = Some(u16le(buf, o));
            ev.value = Some(buf[o + 2]);
        }
        OP_DRIVE_HEAD => {
            ev.halftrack = Some(buf[o]);
            // 0xff = "between sectors" (rotation gap) — a SENTINEL, not a
            // sector number. Kept raw; interpretation belongs to the caller.
            ev.sector = Some(buf[o + 1]);
        }
        OP_BLOCK_READ => {
            ev.halftrack = Some(buf[o]);
            ev.sector = Some(buf[o + 1]);
            ev.bytes = Some(u16le(buf, o + 2) as u32);
        }
        OP_CART_READ => {
            ev.bank = Some(u16le(buf, o));
            ev.slot = Some(buf[o + 2]);
            ev.off_lo = Some(u16le(buf, o + 3));
            ev.off_hi = Some(u16le(buf, o + 5));
            ev.bytes = Some(u32::from_le_bytes([
                buf[o + 7],
                buf[o + 8],
                buf[o + 9],
                buf[o + 10],
            ]));
        }
        OP_MARK => {
            let len = u16le(buf, o) as usize;
            o += 2;
            // The TS encoder byte-truncates the label at 200 bytes, so it CAN
            // end mid-UTF-8-sequence. Decode lossily, never fail.
            ev.label = Some(String::from_utf8_lossy(&buf[o..o + len]).into_owned());
        }
        // 0x21 CIA_EVENT / 0x32 VIA_REG_WRITE / 0x33 GCR_EVENT: skip-only.
        // No field decoder exists on either side; emit {op, cycle} and advance.
        _ => {}
    }
    Ok(Step::Event { ev, next })
}

/// Decode a complete in-memory event stream starting at `start`.
/// Stops at the first incomplete record (truncated tail) or EOF.
pub fn decode_event_stream(buf: &[u8], start: usize, version: u16) -> Result<Vec<DecodedEvent>> {
    let mut out = Vec::new();
    let mut off = start;
    while let Step::Event { ev, next } = decode_event(buf, off, version)? {
        out.push(ev);
        off = next;
    }
    Ok(out)
}

// ── Streaming reader ─────────────────────────────────────────────────────────

/// Header read window. The reference reader parses the header from the first
/// `min(file_len, 16 MiB)` bytes and SEEDS the event stream with whatever of
/// that window lies past `header_len`.
pub fn index_header_max() -> usize {
    env_bytes("INDEX_HEADER_BYTES").unwrap_or(16 * 1024 * 1024).max(4096)
}

/// Streaming window size. Default 256 MiB (reference: `C64RE_INDEX_WINDOW_BYTES`).
/// We allocate `min(window, remaining + carry)` rather than always 256 MiB —
/// behaviour-neutral, per Spec 802 R2 §3.5.
pub fn index_window_bytes() -> usize {
    env_bytes("INDEX_WINDOW_BYTES").unwrap_or(256 * 1024 * 1024).max(64 * 1024)
}

/// Carry guard: more than this many undecodable bytes mid-file ⇒ corrupt.
/// The reference uses 4096 (deliberately loose vs the 211-byte real max);
/// matching it keeps the corrupt-file error identical and cannot false-positive
/// on anything either writer emits.
pub const MAX_EVENT_BYTES: usize = 4096;

/// Read `TRX64_<name>` then `C64RE_<name>` (the reference env names). These
/// knobs exist ONLY to force the cross-window carry path in tests — without
/// them a boundary-straddling record needs a multi-GB fixture to exercise.
fn env_bytes(name: &str) -> Option<usize> {
    for prefix in ["TRX64_", "C64RE_"] {
        if let Ok(v) = std::env::var(format!("{prefix}{name}")) {
            if let Ok(n) = v.trim().parse::<usize>() {
                if n > 0 {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Fill `buf[at..at+len]` from `pos`, looping past short reads. Returns the
/// count actually read (< len only at EOF).
fn read_full_at(f: &mut File, buf: &mut [u8], pos: u64) -> Result<usize> {
    f.seek(SeekFrom::Start(pos)).map_err(|e| TraceReadError::io("seek .c64retrace", e))?;
    let mut got = 0usize;
    while got < buf.len() {
        let n = f
            .read(&mut buf[got..])
            .map_err(|e| TraceReadError::io("read .c64retrace", e))?;
            if n == 0 {
                break;
            }
        got += n;
    }
    Ok(got)
}

/// Summary of a full scan (what the reader loop learned along the way).
#[derive(Debug, Clone, Default)]
pub struct ScanSummary {
    /// Cycle of the LAST decoded record — including MARK and dropped/reserved
    /// records. Seeded from `meta.cycleStart` for a zero-event file.
    pub last_cycle: f64,
    /// Every record decoded, MARK and reserved included.
    pub records: u64,
    /// `.c64retrace` size in bytes (becomes `trace_run.bytes_written`).
    pub bytes: u64,
}

/// A streaming `.c64retrace` reader.
///
/// [`open`](RetraceReader::open) parses the file header (a bounded read — never
/// the whole log) so the caller has `run_id`, `version`, `cycle_start` and the
/// definition **before** any event is delivered; [`stream`](RetraceReader::stream)
/// then walks every record in file order.
///
/// The loop reproduces the reference reader's EOF contract exactly:
///
/// * an incomplete record with more file left ⇒ carry the tail into the next window;
/// * an incomplete record at EOF ⇒ **drop it silently** and finish successfully
///   (a truncated final record from an aborted trace is NOT an error);
/// * more than [`MAX_EVENT_BYTES`] undecodable bytes MID-file ⇒ corrupt, error.
///
/// A header-only file (zero events) is valid and yields no callbacks.
pub struct RetraceReader {
    file: File,
    path: PathBuf,
    size: u64,
    header: ParsedHeader,
    /// Bytes of the header window that lie past `header_len` — already-read
    /// events that SEED the stream and must not be re-read.
    seed_carry: Vec<u8>,
    /// File position immediately after the header window, i.e. where
    /// [`stream`](RetraceReader::stream) resumes reading.
    ///
    /// Captured at open time and NEVER recomputed: deriving it from
    /// `index_header_max()` a second time would desync `seed_carry` from the
    /// file position if the env knob changed in between (and it would silently
    /// skip or double-read a stretch of events).
    next_pos: u64,
}

impl RetraceReader {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).map_err(|e| TraceReadError::io_at("open", path, e))?;
        let size = file
            .metadata()
            .map_err(|e| TraceReadError::io_at("stat", path, e))?
            .len();
        let hn = std::cmp::min(size as usize, index_header_max());
        let mut hbuf = vec![0u8; hn];
        let got = read_full_at(&mut file, &mut hbuf, 0)?;
        hbuf.truncate(got);
        let header = decode_file_header(&hbuf)?;
        let seed_carry = if hbuf.len() > header.header_len {
            hbuf[header.header_len..].to_vec()
        } else {
            Vec::new()
        };
        let next_pos = got as u64;
        Ok(RetraceReader { file, path: path.to_path_buf(), size, header, seed_carry, next_pos })
    }

    pub fn header(&self) -> &ParsedHeader {
        &self.header
    }
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Walk every record through `on_event`, in file order.
    pub fn stream<F>(mut self, mut on_event: F) -> Result<ScanSummary>
    where
        F: FnMut(&DecodedEvent),
    {
        let mut summary = ScanSummary {
            last_cycle: self.header.meta.cycle_start,
            records: 0,
            bytes: self.size,
        };
        let mut carry = std::mem::take(&mut self.seed_carry);
        let mut file_pos = self.next_pos;
        let window_cap = index_window_bytes();
        let version = self.header.version;

        loop {
            let remaining = self.size.saturating_sub(file_pos) as usize;
            let space = window_cap.saturating_sub(carry.len());
            let to_read = std::cmp::min(space, remaining);
            let mut window = Vec::with_capacity(carry.len() + to_read);
            window.extend_from_slice(&carry);
            if to_read > 0 {
                let base = window.len();
                window.resize(base + to_read, 0);
                let got = read_full_at(&mut self.file, &mut window[base..], file_pos)?;
                window.truncate(base + got);
                file_pos += got as u64;
            }
            if window.is_empty() {
                break;
            }

            let mut off = 0usize;
            while let Step::Event { ev, next } = decode_event(&window, off, version)? {
                summary.last_cycle = ev.cycle;
                summary.records += 1;
                on_event(&ev);
                off = next;
            }

            let tail_len = window.len() - off;
            if file_pos >= self.size {
                // EOF: a leftover tail is a truncated final record (aborted
                // trace). Drop it and finish successfully.
                break;
            }
            if tail_len > MAX_EVENT_BYTES {
                return Err(TraceReadError::CorruptTail {
                    bytes: tail_len,
                    offset: file_pos,
                    path: self.path.display().to_string(),
                });
            }
            carry = window[off..].to_vec();
        }
        Ok(summary)
    }
}

/// Convenience: open + stream in one call.
pub fn stream_retrace<F>(path: &Path, on_event: F) -> Result<(ParsedHeader, ScanSummary)>
where
    F: FnMut(&DecodedEvent),
{
    let r = RetraceReader::open(path)?;
    let header = r.header.clone();
    let summary = r.stream(on_event)?;
    Ok((header, summary))
}

/// Read ONLY the file header — never the whole log.
pub fn read_retrace_meta(path: &Path) -> Result<ParsedHeader> {
    Ok(RetraceReader::open(path)?.header)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-exact fixture builder mirroring `FrameSink` — used by the unit
    /// tests here and by the integration tests.
    pub(crate) struct Fixture {
        pub buf: Vec<u8>,
        pub version: u16,
    }

    impl Fixture {
        pub fn new(meta_json: &str, version: u16) -> Self {
            let mut buf = Vec::new();
            buf.extend_from_slice(&MAGIC[..]);
            buf.extend_from_slice(&version.to_le_bytes());
            buf.extend_from_slice(&0u16.to_le_bytes());
            buf.extend_from_slice(&(meta_json.len() as u32).to_le_bytes());
            buf.extend_from_slice(meta_json.as_bytes());
            Self { buf, version }
        }
        fn cyc(&mut self, c: u64) {
            self.buf.extend_from_slice(&(c as f64).to_le_bytes());
        }
        pub fn cpu(&mut self, op: u8, c: u64, pc: u16, regs: [u8; 8]) {
            self.buf.push(op);
            self.cyc(c);
            self.buf.extend_from_slice(&pc.to_le_bytes());
            self.buf.extend_from_slice(&regs);
        }
        pub fn mem(&mut self, op: u8, c: u64, addr: u16, val: u8, pc: u16, acc: u8, old: Option<u8>) {
            self.buf.push(op);
            self.cyc(c);
            self.buf.extend_from_slice(&addr.to_le_bytes());
            self.buf.push(val);
            self.buf.extend_from_slice(&pc.to_le_bytes());
            if self.version < 2 {
                self.buf.push(acc);
            } else {
                self.buf.push((acc & 0x7f) | if old.is_some() { 0x80 } else { 0 });
                self.buf.push(old.unwrap_or(0));
            }
        }
        pub fn mark(&mut self, c: u64, label: &str) {
            self.buf.push(OP_MARK);
            self.cyc(c);
            let b = label.as_bytes();
            self.buf.extend_from_slice(&(b.len() as u16).to_le_bytes());
            self.buf.extend_from_slice(b);
        }
        pub fn iec(&mut self, c: u64, lines: u16) {
            self.buf.push(OP_IEC_LINE_CHANGE);
            self.cyc(c);
            self.buf.extend_from_slice(&lines.to_le_bytes());
        }
        pub fn vic(&mut self, c: u64, raster_y: u16, kind: u8, value: u8) {
            self.buf.push(OP_VIC_REG_WRITE);
            self.cyc(c);
            self.buf.extend_from_slice(&raster_y.to_le_bytes());
            self.buf.push(kind);
            self.buf.push(value);
        }
        pub fn sid(&mut self, c: u64, reg: u16, value: u8) {
            self.buf.push(OP_SID_REG_WRITE);
            self.cyc(c);
            self.buf.extend_from_slice(&reg.to_le_bytes());
            self.buf.push(value);
        }
        pub fn reserved(&mut self, op: u8, c: u64) {
            self.buf.push(op);
            self.cyc(c);
            self.buf.extend_from_slice(&[0u8; 4]); // 12-byte payload total
        }
        pub fn drive_head(&mut self, c: u64, ht: u8, sector: u8) {
            self.buf.push(OP_DRIVE_HEAD);
            self.cyc(c);
            self.buf.push(ht);
            self.buf.push(sector);
        }
        pub fn block_read(&mut self, c: u64, ht: u8, sector: u8, bytes: u16) {
            self.buf.push(OP_BLOCK_READ);
            self.cyc(c);
            self.buf.push(ht);
            self.buf.push(sector);
            self.buf.extend_from_slice(&bytes.to_le_bytes());
        }
        #[allow(clippy::too_many_arguments)]
        pub fn cart_read(
            &mut self,
            c: u64,
            bank: u16,
            slot: u8,
            off_lo: u16,
            off_hi: u16,
            bytes: u32,
        ) {
            self.buf.push(OP_CART_READ);
            self.cyc(c);
            self.buf.extend_from_slice(&bank.to_le_bytes());
            self.buf.push(slot);
            self.buf.extend_from_slice(&off_lo.to_le_bytes());
            self.buf.extend_from_slice(&off_hi.to_le_bytes());
            self.buf.extend_from_slice(&bytes.to_le_bytes());
        }
    }

    const META: &str = r#"{"runId":"run_x","defId":"live-capture","defVersion":1,"defName":"live session capture","defJson":"{\"id\":\"live-capture\",\"version\":1,\"name\":\"live session capture\",\"retention\":\"transient\"}","domains":["c64-cpu","memory"],"cycleStart":100,"createdAt":"2026-08-08T00:00:00.000Z"}"#;

    #[test]
    fn header_round_trip_and_meta_fields() {
        let f = Fixture::new(META, 2);
        let h = decode_file_header(&f.buf).unwrap();
        assert_eq!(h.version, 2);
        assert_eq!(h.header_len, 16 + META.len());
        assert_eq!(h.meta.run_id, "run_x");
        assert_eq!(h.meta.def_id, "live-capture");
        assert_eq!(h.meta.cycle_start, 100.0);
        assert!(h.meta.media_sha.is_none());
        let d = h.definition().unwrap();
        assert_eq!(d.name, "live session capture");
        assert_eq!(d.retention, "transient");
        assert!(!d.synthesized);
        // def_json is stored VERBATIM (R2 J-2), not re-serialized.
        assert_eq!(d.raw, h.meta.def_json);
    }

    #[test]
    fn header_errors_are_verbatim() {
        assert_eq!(
            decode_file_header(&[0u8; 4]).unwrap_err().to_string(),
            "c64retrace: truncated header"
        );
        let mut bad = Fixture::new(META, 2).buf;
        bad[0] = b'X';
        assert_eq!(decode_file_header(&bad).unwrap_err().to_string(), "c64retrace: bad magic");
        let f = Fixture::new(META, 3);
        assert_eq!(
            decode_file_header(&f.buf).unwrap_err().to_string(),
            "c64retrace: unsupported format version 3 (this build reads v1..v2)"
        );
        let f0 = Fixture::new(META, 0);
        assert_eq!(
            decode_file_header(&f0.buf).unwrap_err().to_string(),
            "c64retrace: unsupported format version 0 (this build reads v1..v2)"
        );
        let f = Fixture::new(META, 2);
        assert_eq!(
            decode_file_header(&f.buf[..20]).unwrap_err().to_string(),
            "c64retrace: truncated meta"
        );
    }

    #[test]
    fn defjson_empty_is_lenient_by_default() {
        let meta = r#"{"runId":"r","defId":"capsel","defVersion":1,"defName":"","defJson":"","domains":["c64-cpu"],"cycleStart":0,"createdAt":""}"#;
        let f = Fixture::new(meta, 2);
        let h = decode_file_header(&f.buf).unwrap();
        let d = h.definition().unwrap();
        assert!(d.synthesized, "empty defJson must synthesize, not throw (R1 §7.1)");
        assert_eq!(d.name, "capsel");
        // and the synthesized raw is valid JSON
        serde_json::from_str::<serde_json::Value>(&d.raw).unwrap();
    }

    #[test]
    fn cycle_is_f64_not_u64_bits() {
        let mut f = Fixture::new(META, 2);
        f.drive_head(0x0102_0304_0506_0708, 71, 17);
        let evs = decode_event_stream(&f.buf, 16 + META.len(), 2).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].cycle, 0x0102_0304_0506_0708u64 as f64);
        assert_eq!(evs[0].halftrack, Some(71));
        assert_eq!(evs[0].sector, Some(17));
    }

    #[test]
    fn v2_mem_old_value_gated_on_present_bit() {
        let mut f = Fixture::new(META, 2);
        f.mem(OP_RAM_WRITE, 10, 0x0400, 0x41, 0xc000, ACCESS_WRITE, Some(0x20));
        f.mem(OP_RAM_WRITE, 11, 0xd020, 0x06, 0xc003, ACCESS_WRITE, None);
        f.mem(OP_RAM_WRITE, 12, 0x0400, 0x41, 0xc006, ACCESS_READ, None);
        let evs = decode_event_stream(&f.buf, 16 + META.len(), 2).unwrap();
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0].old_value, Some(0x20));
        assert_eq!(evs[0].access, Some(ACCESS_WRITE));
        assert_eq!(evs[1].old_value, None, "I/O-window write carries no old byte");
        assert_eq!(evs[2].old_value, None, "reads never carry it");
        assert_eq!(evs[2].access, Some(ACCESS_READ));
    }

    #[test]
    fn v1_mem_record_is_one_byte_shorter() {
        let mut f = Fixture::new(META, 1);
        f.mem(OP_RAM_WRITE, 10, 0x0400, 0x41, 0xc000, ACCESS_WRITE, None);
        f.cpu(OP_CPU_STEP, 11, 0xc003, [0xa9, 1, 2, 3, 4, 5, 6, 7]);
        let start = 16 + META.len();
        // 16-byte v2 layout would misalign; at v1 the record is 15 bytes.
        assert_eq!(f.buf.len() - start, 15 + 19);
        let evs = decode_event_stream(&f.buf, start, 1).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].addr, Some(0x0400));
        assert_eq!(evs[0].access, Some(ACCESS_WRITE));
        assert_eq!(evs[0].old_value, None);
        assert_eq!(evs[1].pc, Some(0xc003));
        assert_eq!(evs[1].opcode, Some(0xa9));
    }

    #[test]
    fn truncated_final_record_is_incomplete_not_panic() {
        let mut f = Fixture::new(META, 2);
        f.cpu(OP_CPU_STEP, 1, 0xc000, [1, 2, 3, 4, 5, 6, 7, 8]);
        f.cpu(OP_CPU_STEP, 2, 0xc003, [1, 2, 3, 4, 5, 6, 7, 8]);
        let start = 16 + META.len();
        for cut in 1..19usize {
            let short = &f.buf[..f.buf.len() - cut];
            let evs = decode_event_stream(short, start, 2).unwrap();
            assert_eq!(evs.len(), 1, "cut={cut}: only the complete record decodes");
        }
    }

    #[test]
    fn mark_bounds_gate_before_label_read() {
        let mut f = Fixture::new(META, 2);
        f.mark(5, "phase-one");
        let start = 16 + META.len();
        // Cut inside the label, and inside the length prefix.
        for cut in [1usize, 5, 9, 11] {
            let short = &f.buf[..f.buf.len() - cut];
            assert!(matches!(decode_event(short, start, 2).unwrap(), Step::Incomplete));
        }
        let evs = decode_event_stream(&f.buf, start, 2).unwrap();
        assert_eq!(evs[0].label.as_deref(), Some("phase-one"));
    }

    #[test]
    fn mark_with_split_utf8_decodes_lossily() {
        let mut f = Fixture::new(META, 2);
        // Hand-build a MARK whose label ends in a split multi-byte sequence.
        f.buf.push(OP_MARK);
        f.buf.extend_from_slice(&(7u64 as f64).to_le_bytes());
        let bad = [b'o', b'k', 0xe2, 0x82]; // truncated U+20AC
        f.buf.extend_from_slice(&(bad.len() as u16).to_le_bytes());
        f.buf.extend_from_slice(&bad);
        let evs = decode_event_stream(&f.buf, 16 + META.len(), 2).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(evs[0].label.as_deref().unwrap().starts_with("ok"));
    }

    #[test]
    fn media_write_and_unknown_opcodes_are_hard_errors() {
        let mut f = Fixture::new(META, 2);
        let start = 16 + META.len();
        f.buf.push(OP_MEDIA_WRITE);
        f.buf.extend_from_slice(&[0u8; 32]);
        let e = decode_event(&f.buf, start, 2).unwrap_err();
        assert_eq!(e.to_string(), format!("c64retrace: cannot skip opcode 0x40 at {start}"));

        let mut g = Fixture::new(META, 2);
        g.buf.push(0x7f);
        g.buf.extend_from_slice(&[0u8; 32]);
        assert!(matches!(
            decode_event(&g.buf, start, 2),
            Err(TraceReadError::UnskippableOpcode { op: 0x7f, .. })
        ));
    }

    #[test]
    fn reserved_opcodes_decode_to_op_and_cycle_only() {
        let mut f = Fixture::new(META, 2);
        for op in [OP_CIA_EVENT, OP_VIA_REG_WRITE, OP_GCR_EVENT] {
            f.reserved(op, 42);
        }
        let evs = decode_event_stream(&f.buf, 16 + META.len(), 2).unwrap();
        assert_eq!(evs.len(), 3);
        for ev in &evs {
            assert_eq!(ev.cycle, 42.0);
            assert!(ev.pc.is_none() && ev.addr.is_none() && ev.value.is_none());
        }
    }

    #[test]
    fn record_widths_match_the_spec_table() {
        for (op, want_v2, want_v1) in [
            (OP_CPU_STEP, 19usize, 19usize),
            (OP_DRIVE_CPU_STEP, 19, 19),
            (OP_RAM_WRITE, 16, 15),
            (OP_IO_WRITE, 16, 15),
            (OP_DRIVE_RAM_WRITE, 16, 15),
            (OP_VIC_REG_WRITE, 13, 13),
            (OP_CIA_EVENT, 13, 13),
            (OP_SID_REG_WRITE, 12, 12),
            (OP_IEC_LINE_CHANGE, 11, 11),
            (OP_VIA_REG_WRITE, 13, 13),
            (OP_GCR_EVENT, 13, 13),
            (OP_DRIVE_HEAD, 11, 11),
            (OP_BLOCK_READ, 13, 13),
        ] {
            assert_eq!(payload_size(op, 2).unwrap() + 1, want_v2, "op {op:#x} @v2");
            assert_eq!(payload_size(op, 1).unwrap() + 1, want_v1, "op {op:#x} @v1");
        }
        assert!(payload_size(OP_MARK, 2).is_none());
        assert!(payload_size(OP_MEDIA_WRITE, 2).is_none());
    }

    #[test]
    fn iec_bits_and_vic_sid_fields() {
        let mut f = Fixture::new(META, 2);
        f.iec(7, IEC_BIT_ATN | IEC_BIT_DRV_ATN_ACK);
        f.vic(8, 51, 4, 0x1b);
        f.sid(9, 0xd404, 0x21);
        let evs = decode_event_stream(&f.buf, 16 + META.len(), 2).unwrap();
        assert_eq!(evs[0].lines, Some(IEC_BIT_ATN | IEC_BIT_DRV_ATN_ACK));
        assert_eq!(evs[1].raster_y, Some(51));
        assert_eq!(evs[1].kind_code, Some(4));
        assert_eq!(evs[1].value, Some(0x1b));
        assert_eq!(evs[2].reg, Some(0xd404));
        assert_eq!(evs[2].value, Some(0x21));
    }

    #[test]
    fn block_read_fields() {
        let mut f = Fixture::new(META, 2);
        f.block_read(1234, 70, 17, 0x0154);
        let evs = decode_event_stream(&f.buf, 16 + META.len(), 2).unwrap();
        assert_eq!(evs[0].halftrack, Some(70));
        assert_eq!(evs[0].sector, Some(17));
        assert_eq!(evs[0].bytes, Some(0x0154));
    }

    /// Spec 785 C1 — CART_READ (0x36) decodes its six fields, and its 20-byte
    /// width keeps a following record aligned (a wrong `payload_size` misaligns
    /// the rest of the stream, which is the failure this pins).
    #[test]
    fn cart_read_fields_and_width() {
        let mut f = Fixture::new(META, 2);
        f.cart_read(5000, 0x002e, 1, 0x0040, 0x1fff, 0x0001_2345);
        f.cpu(OP_CPU_STEP, 5001, 0xc000, [1, 2, 3, 4, 5, 6, 7, 8]);
        let evs = decode_event_stream(&f.buf, 16 + META.len(), 2).unwrap();
        assert_eq!(evs.len(), 2, "the 20-byte record left the next one aligned");
        assert_eq!(evs[0].op, OP_CART_READ);
        assert_eq!(evs[0].clock(), 5000);
        assert_eq!(evs[0].bank, Some(0x002e));
        assert_eq!(evs[0].slot, Some(1));
        assert_eq!(evs[0].off_lo, Some(0x0040));
        assert_eq!(evs[0].off_hi, Some(0x1fff));
        assert_eq!(evs[0].bytes, Some(0x0001_2345));
        assert_eq!(evs[1].pc, Some(0xc000));
        // The disk lane's fields stay absent on a cart record and vice versa.
        assert_eq!(evs[0].halftrack, None);
        assert_eq!(evs[0].sector, None);
    }

    /// Regression: the reader must capture its resume position at OPEN time.
    /// Recomputing it from `index_header_max()` inside `stream()` desyncs the
    /// seed carry from the file position whenever the env knob changed in
    /// between — which silently skips (or double-reads) a stretch of events.
    #[test]
    fn resume_position_survives_a_header_window_change_mid_read() {
        let mut f = Fixture::new(META, 2);
        for i in 0..300u64 {
            f.cpu(OP_CPU_STEP, 1000 + i, 0xc000 + i as u16, [1, 2, 3, 4, 5, 6, 7, 8]);
        }
        let mut path = std::env::temp_dir();
        path.push(format!("trx64-resume-{}.c64retrace", std::process::id()));
        std::fs::write(&path, &f.buf).unwrap();

        std::env::set_var("TRX64_INDEX_HEADER_BYTES", "4096");
        let r = RetraceReader::open(&path).unwrap();
        // Simulate the knob changing between open() and stream().
        std::env::set_var("TRX64_INDEX_HEADER_BYTES", "8192");
        let mut n = 0u64;
        let s = r.stream(|_| n += 1).unwrap();
        std::env::remove_var("TRX64_INDEX_HEADER_BYTES");
        let _ = std::fs::remove_file(&path);

        assert_eq!(n, 300, "no event skipped or duplicated");
        assert_eq!(s.records, 300);
    }

    #[test]
    fn zero_event_file_is_valid() {
        let f = Fixture::new(META, 2);
        let evs = decode_event_stream(&f.buf, 16 + META.len(), 2).unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn clock_truncates_toward_zero() {
        let ev = DecodedEvent { cycle: 12345.9, ..Default::default() };
        assert_eq!(ev.clock(), 12345);
    }
}
