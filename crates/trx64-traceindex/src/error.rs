//! Shared error type for the whole native trace-read stack (Spec 802).
//!
//! Every module in this crate — `decode`, `schema`, `build`, `ensure`, and the
//! stage-2 renderers (`queries`, `map`, `taint`, `swimlane`) — returns
//! [`Result<T>`]. The daemon turns a `TraceReadError` into a WS error string via
//! `Display`, so the MESSAGE TEXT IS PART OF THE PARITY CONTRACT: the sidecar's
//! messages are reproduced verbatim where Spec 802 R1/R2 pin them
//! (`c64retrace: bad magic`, `c64retrace: truncated header`,
//! `c64retrace: truncated meta`, the unsupported-version message, the
//! `cannot skip opcode 0x??` message, the corrupt-tail message, and the
//! "still building" retry message).
//!
//! **DuckDB errors are surfaced BARE (Spec 802 F1).** The sidecar handed the
//! node driver's `e.message` straight to the caller, so a Rust-side
//! `"{what}: "` prefix in front of `Binder Error: …` / `IO Error: …` is a
//! measurable parity break. [`TraceReadError::Duck`] therefore renders only its
//! source and keeps `what` for `Debug` / [`TraceReadError::context`];
//! [`TraceReadError::DuckBuild`] — the index-build path, which has no sidecar
//! counterpart at all — is the single opt-in that still renders context.

use std::fmt;
use std::path::Path;

pub type Result<T> = std::result::Result<T, TraceReadError>;

#[derive(Debug)]
pub enum TraceReadError {
    /// Filesystem failure (open / stat / read / rename / unlink).
    Io { what: String, source: std::io::Error },

    // ── `.c64retrace` format errors — messages pinned by Spec 802 R1 §6 ──────
    /// `c64retrace: truncated header` — file shorter than the 16 fixed bytes.
    TruncatedHeader,
    /// `c64retrace: truncated meta` — file shorter than 16 + meta_len.
    TruncatedMeta,
    /// `c64retrace: bad magic` — the first 8 bytes are not `C64RETR1`.
    BadMagic,
    /// `c64retrace: unsupported format version N (this build reads v1..v2)`.
    UnsupportedVersion(u16),
    /// `c64retrace: cannot skip opcode 0x?? at <off>` — 0x40 / unknown opcode.
    /// The size table is the only skip mechanism and has no entry, so the
    /// stream cannot be resynchronised. Hard failure, exactly like the TS
    /// reference decoder.
    UnskippableOpcode { op: u8, off: usize },
    /// The header meta JSON did not parse (never produced by either writer).
    BadMeta(String),
    /// `trace index: <n> undecodable bytes near offset <p> of <path> (corrupt .c64retrace?)`
    CorruptTail { bytes: usize, offset: u64, path: String },

    // ── index / store errors ────────────────────────────────────────────────
    /// A DuckDB call on a path the sidecar ALSO had (open / prepare / run /
    /// fetch / column-read, and the read-path compat-view heal) failed.
    ///
    /// **`Display` renders the BARE driver message.** `what` is kept, but as
    /// DIAGNOSTIC CONTEXT ONLY — it reaches `Debug`, `{:?}` logs and
    /// [`TraceReadError::context`], never a caller. Spec 802 F1: the sidecar
    /// surfaced `e.message` from the node driver verbatim
    /// (`{"error": e.message}`), so any Rust-side `"{what}: "` prefix here is a
    /// parity break — measured as
    /// `prepare query: Binder Error: …` vs the sidecar's `Binder Error: …`, and
    /// `open trace store …: IO Error: …` vs the sidecar's `IO Error: …`.
    ///
    /// This is the DEFAULT and the parity-safe one: a new DuckDB call site added
    /// on a read path gets bare text without the author having to know the rule.
    /// Opt INTO context only via [`TraceReadError::duck_build`], and only where
    /// no sidecar counterpart exists.
    Duck { what: String, source: duckdb::Error },

    /// A DuckDB call inside the INDEX BUILD failed — a NATIVE-ONLY failure mode.
    ///
    /// The sidecar's `index` op delegated to C64RE's TypeScript indexer, whose
    /// internal failure text is a different implementation's text entirely;
    /// there is no string to be byte-equal to. So `Display` KEEPS the context —
    /// it names which of the dozen build steps (`open trace_event appender`,
    /// `append trace_event row`, `insert trace_run`, `close index store`, …)
    /// failed, which is the only thing that makes a build failure actionable.
    DuckBuild { what: String, source: duckdb::Error },
    /// The index build for this path failed earlier; message carried forward.
    IndexUnavailable { path: String, why: String },
    /// `ensure_index_bounded` gave up waiting — the build continues.
    IndexStillBuilding { path: String, waited_ms: u64 },
    /// A `wait: true` build finished but produced no store.
    IndexProducedNoStore { path: String, why: Option<String> },
    /// Neither a store nor a `.c64retrace` authority exists at the given stem.
    NoAuthority { duckdb: String, retrace: String },

    /// Anything else the reader ops need to report (bad argument, unknown fn).
    Other(String),
}

impl TraceReadError {
    pub fn io(what: impl Into<String>, source: std::io::Error) -> Self {
        TraceReadError::Io { what: what.into(), source }
    }
    /// A DuckDB failure on a READ path. `what` is recorded for logs; the
    /// caller-visible `Display` is the bare driver message (Spec 802 F1).
    pub fn duck(what: impl Into<String>, source: duckdb::Error) -> Self {
        TraceReadError::Duck { what: what.into(), source }
    }
    /// A DuckDB failure inside the index BUILD — native-only, so `what` is part
    /// of the rendered message. Never use this on a read path.
    pub fn duck_build(what: impl Into<String>, source: duckdb::Error) -> Self {
        TraceReadError::DuckBuild { what: what.into(), source }
    }
    pub fn other(msg: impl Into<String>) -> Self {
        TraceReadError::Other(msg.into())
    }
    pub fn io_at(what: &str, path: &Path, source: std::io::Error) -> Self {
        TraceReadError::Io { what: format!("{what} {}", path.display()), source }
    }

    /// The internal context of an error whose `Display` may not carry it.
    ///
    /// This is where the `what` of a [`TraceReadError::Duck`] goes when the
    /// parity contract forbids it in the message: `Display` stays bare for the
    /// caller, `context()` (and `{:?}`) keeps it for the daemon's own log.
    pub fn context(&self) -> Option<&str> {
        match self {
            TraceReadError::Io { what, .. }
            | TraceReadError::Duck { what, .. }
            | TraceReadError::DuckBuild { what, .. } => Some(what.as_str()),
            _ => None,
        }
    }
}

impl fmt::Display for TraceReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use TraceReadError::*;
        match self {
            Io { what, source } => write!(f, "{what}: {source}"),
            TruncatedHeader => write!(f, "c64retrace: truncated header"),
            TruncatedMeta => write!(f, "c64retrace: truncated meta"),
            BadMagic => write!(f, "c64retrace: bad magic"),
            UnsupportedVersion(v) => write!(
                f,
                "c64retrace: unsupported format version {v} (this build reads v1..v{})",
                crate::decode::FORMAT_VERSION
            ),
            UnskippableOpcode { op, off } => {
                write!(f, "c64retrace: cannot skip opcode 0x{op:x} at {off}")
            }
            BadMeta(m) => write!(f, "c64retrace: bad meta json: {m}"),
            CorruptTail { bytes, offset, path } => write!(
                f,
                "trace index: {bytes} undecodable bytes near offset {offset} of {path} (corrupt .c64retrace?)"
            ),
            // Spec 802 F1 — BARE. The sidecar surfaced the driver's own message
            // with no prefix; `what` lives on for `Debug` / `context()`.
            Duck { source, .. } => write!(f, "{source}"),
            // Native-only (index build): context is kept, see the variant doc.
            DuckBuild { what, source } => write!(f, "{what}: {source}"),
            IndexUnavailable { path, why } => {
                write!(f, "trace index unavailable for {path}: {why}")
            }
            IndexStillBuilding { path, waited_ms } => write!(
                f,
                "trace index for {path} is still building (background decode of the .c64retrace \
                 authority — waited {}s, large traces take longer). The build continues on a \
                 worker thread; retry this tool in ~30s and it will read the finished store.",
                (*waited_ms as f64 / 1000.0).round() as u64
            ),
            IndexProducedNoStore { path, why } => match why {
                Some(w) => write!(f, "index build produced no store at {path}: {w}"),
                None => write!(f, "index build produced no store at {path}"),
            },
            NoAuthority { duckdb, retrace } => write!(
                f,
                "no trace store and no .c64retrace authority at {duckdb} (looked for {retrace})"
            ),
            Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for TraceReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TraceReadError::Io { source, .. } => Some(source),
            TraceReadError::Duck { source, .. } => Some(source),
            TraceReadError::DuckBuild { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// `?` on a bare `duckdb::Error` lands on the READ-path variant, so the implicit
/// conversion cannot smuggle a prefix into a caller-visible message either. The
/// `"duckdb"` marker is context-only (it never renders).
impl From<duckdb::Error> for TraceReadError {
    fn from(e: duckdb::Error) -> Self {
        TraceReadError::Duck { what: "duckdb".into(), source: e }
    }
}
