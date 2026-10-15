//! `index_binary_log` — build (or rebuild) a DuckDB index from a `.c64retrace`.
//!
//! Whole-file, all-or-nothing, into a **temp store in the same directory**,
//! published with a single `rename`. That rename is the atomicity mechanism —
//! there is deliberately **no transaction**: an explicit `BEGIN`/`COMMIT` around
//! tens of millions of appended rows changes the memory profile for nothing.
//!
//! A concurrent reader therefore always sees either the previous complete store
//! or the new one, never a half-written or exclusively-locked file. A failed
//! build is unlinked and never becomes visible; the `.c64retrace` stays the
//! re-indexable authority.

use crate::decode::{self, ParsedHeader};
use crate::error::{Result, TraceReadError};
use crate::media;
use crate::rows::event_to_row;
use crate::schema;
use duckdb::{params, Connection};
use std::path::{Path, PathBuf};

/// Flush the appender every N rows to bound its internal buffer.
const APPENDER_FLUSH: u64 = 50_000;

/// Run fields the `.c64retrace` header (written at START) cannot know.
///
/// The reference `indexBinaryLog(retrace, duckdb, runOverrides?)` merges these
/// LAST over the header-derived run. `finalize_trace` should pass what it
/// already has — today it passes nothing and those fields are silently lost.
#[derive(Debug, Clone, Default)]
pub struct IndexOverrides {
    pub stop_checkpoint_id: Option<String>,
    pub branch_id: Option<String>,
    pub overhead_ms: Option<f64>,
    /// Spec 802 R2 **J-3**: a user's marks live only in `TraceState.marks` and
    /// the stop descriptor, never in the `.c64retrace`; without these
    /// `trace_mark` comes out empty (⇒ the `anchors` view is empty ⇒
    /// `listAnchors`/`findAnchor` return nothing). The only 0x01 records TRX64
    /// writes are the `media/1` records (Spec 909), and those go to
    /// `trace_media`, not here.
    ///
    /// Passing them here matches the TS `runOverrides` mechanism. When
    /// non-empty these REPLACE the marks decoded from the file.
    pub marks: Option<Vec<(u64, String)>>,
}

/// What a completed build reports.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexResult {
    pub run_id: String,
    pub event_count: u64,
    pub mark_count: u64,
    /// Number of DISTINCT channels that received rows (a cardinality, not a map).
    pub channels: usize,
    pub output_path: String,
}

/// `.duckdb` → its `.c64retrace` authority. Strip a trailing `.duckdb` and
/// append `.c64retrace`; if the path does not end in `.duckdb`, append whole.
pub fn retrace_path_for(duckdb_path: &Path) -> PathBuf {
    let s = duckdb_path.to_string_lossy();
    if let Some(stem) = s.strip_suffix(".duckdb") {
        PathBuf::from(format!("{stem}.c64retrace"))
    } else {
        PathBuf::from(format!("{s}.c64retrace"))
    }
}

/// Build a DuckDB index from `retrace_path` and publish it at `duckdb_path`.
///
/// Blocking + CPU/IO heavy: call it on a dedicated `std::thread`, never on an
/// async executor.
pub fn index_binary_log(
    retrace_path: &Path,
    duckdb_path: &Path,
    overrides: Option<&IndexOverrides>,
) -> Result<IndexResult> {
    let tmp_path = temp_store_path(duckdb_path);
    let outcome = build_into(retrace_path, duckdb_path, &tmp_path, overrides);
    match outcome {
        Ok(result) => {
            std::fs::rename(&tmp_path, duckdb_path)
                .map_err(|e| TraceReadError::io_at("publish index", duckdb_path, e))?;
            Ok(result)
        }
        Err(e) => {
            // A failed/partial build is never published.
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

fn temp_store_path(duckdb_path: &Path) -> PathBuf {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    PathBuf::from(format!(
        "{}.idx-{}-{}.tmp",
        duckdb_path.display(),
        std::process::id(),
        millis
    ))
}

/// The build itself, into `tmp_path`. Publishing is the caller's job.
fn build_into(
    retrace_path: &Path,
    duckdb_path: &Path,
    tmp_path: &Path,
    overrides: Option<&IndexOverrides>,
) -> Result<IndexResult> {
    if let Some(dir) = tmp_path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .map_err(|e| TraceReadError::io_at("mkdir for index", dir, e))?;
        }
    }
    // A stale temp from a previous crash would otherwise be opened and appended to.
    let _ = std::fs::remove_file(tmp_path);

    // Open the authority FIRST: the header (and therefore `run_id`) is parsed
    // from a bounded leading read, before a single event is delivered. A bad
    // magic / unsupported version fails here, before any store is created.
    let reader = decode::RetraceReader::open(retrace_path)?;
    let header = reader.header().clone();
    let run_id = header.meta.run_id.clone();

    let conn = Connection::open(tmp_path)
        .map_err(|e| TraceReadError::duck_build(format!("open index store {}", tmp_path.display()), e))?;
    schema::create_trace_run_store(&conn)?;

    // ── stream + append ─────────────────────────────────────────────────────
    // Column order MUST match the trace_event DDL:
    //   run_id, seq, cycle, channel, trigger_kind, capture_kind, data_json
    let mut appender = conn
        .appender("trace_event")
        .map_err(|e| TraceReadError::duck_build("open trace_event appender", e))?;

    let mut seq: u64 = 0;
    let mut event_count: u64 = 0;
    let mut since_flush: u64 = 0;
    let mut marks: Vec<(u64, String)> = Vec::new();
    let mut media_rows: Vec<media::MediaRow> =
        media::rows_from_header(header.meta.media.as_ref(), trunc_u64(header.meta.cycle_start));
    let mut channels: Vec<&'static str> = Vec::new();
    // Append failures are parked here — the streaming closure cannot return one.
    let mut append_err: Option<TraceReadError> = None;

    let stream_res = reader.stream(|ev| {
        if append_err.is_some() {
            return;
        }
        if ev.op == decode::OP_MARK {
            // MARK consumes NO seq and produces no trace_event row. A media record
            // (Spec 909) is a row of `trace_media`, not a mark.
            let label = ev.label.clone().unwrap_or_default();
            match media::row_from_mark(ev.clock(), &label) {
                Some(row) => media_rows.push(row),
                None => marks.push((ev.clock(), label)),
            }
            return;
        }
        let this_seq = seq;
        seq += 1; // consumed even when the row is None (reserved / 0x34 / 0x35 / 0x36 / 0x37)
        let Some(row) = event_to_row(ev, this_seq) else { return };
        if !channels.contains(&row.channel) {
            channels.push(row.channel);
        }
        if let Err(e) = appender.append_row(params![
            run_id.as_str(),
            row.seq,
            row.clock(),
            row.channel,
            row.trigger_kind,
            row.capture_kind,
            row.data_json.as_str(),
        ]) {
            append_err = Some(TraceReadError::duck_build("append trace_event row", e));
            return;
        }
        event_count += 1;
        since_flush += 1;
        if since_flush >= APPENDER_FLUSH {
            if let Err(e) = appender.flush() {
                append_err = Some(TraceReadError::duck_build("flush trace_event appender", e));
                return;
            }
            since_flush = 0;
        }
    });

    let summary = match stream_res {
        Ok(v) => v,
        Err(e) => {
            drop(appender);
            drop(conn);
            return Err(e);
        }
    };
    if let Some(e) = append_err {
        drop(appender);
        drop(conn);
        return Err(e);
    }
    appender
        .flush()
        .map_err(|e| TraceReadError::duck_build("final flush trace_event appender", e))?;
    drop(appender);

    // ── run header, marks, meta ─────────────────────────────────────────────
    if let Some(ov) = overrides {
        if let Some(m) = &ov.marks {
            if !m.is_empty() {
                marks = m.clone();
            }
        }
    }
    let mark_count = marks.len() as u64;
    write_trace_run_header(&conn, &header, &summary, event_count, &marks, overrides)?;
    write_trace_media(&conn, &run_id, &media_rows)?;

    // Fold the WAL into the .duckdb BEFORE closing, so a SEPARATE process
    // reading the file sees the rows. Skipping this reproduces the historical
    // "reader sees an empty store with a dangling .duckdb.wal" bug.
    let _ = conn.execute_batch("CHECKPOINT");
    conn.close()
        .map_err(|(_, e)| TraceReadError::duck_build("close index store", e))?;

    Ok(IndexResult {
        run_id,
        event_count,
        mark_count,
        channels: channels.len(),
        output_path: duckdb_path.display().to_string(),
    })
}

/// Spec 909 — the `trace_media` rows: the header's start state, then the records.
fn write_trace_media(conn: &Connection, run_id: &str, rows: &[media::MediaRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut stmt = conn
        .prepare("INSERT INTO trace_media VALUES (?,?,?,?,?,?,?,?,?,?,?)")
        .map_err(|e| TraceReadError::duck_build("prepare trace_media insert", e))?;
    for r in rows {
        stmt.execute(params![
            run_id,
            r.cycle,
            r.slot.as_deref(),
            r.action.as_str(),
            r.sha256.as_deref(),
            r.format.as_deref(),
            r.source_name.as_deref(),
            r.hw_type,
            r.mapper_type.as_deref(),
            r.banks,
            r.crt_name.as_deref(),
        ])
        .map_err(|e| TraceReadError::duck_build("insert trace_media", e))?;
    }
    Ok(())
}

/// Write the single `trace_run` row, the `trace_mark` rows and the 8 `meta`
/// rows — after the appender is closed, all as plain INSERTs with bound
/// parameters.
fn write_trace_run_header(
    conn: &Connection,
    header: &ParsedHeader,
    summary: &decode::ScanSummary,
    event_count: u64,
    marks: &[(u64, String)],
    overrides: Option<&IndexOverrides>,
) -> Result<()> {
    let meta = &header.meta;
    let def = header.definition()?;
    let created_at = now_iso8601();

    let stop_checkpoint_id = overrides.and_then(|o| o.stop_checkpoint_id.clone());
    let branch_id = overrides.and_then(|o| o.branch_id.clone());
    let overhead_ms = overrides.and_then(|o| o.overhead_ms);

    conn.execute(
        "INSERT INTO trace_run VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            meta.run_id.as_str(),
            meta.def_id.as_str(),
            meta.def_version.trunc() as i32,
            def.raw.as_str(),
            def.name.as_str(),
            meta.start_checkpoint_id.as_deref(),
            stop_checkpoint_id.as_deref(),
            meta.media_sha.as_deref(),
            meta.media_name.as_deref(),
            branch_id.as_deref(),
            trunc_u64(meta.cycle_start),
            // `cycle_end` = the cycle of the LAST DECODED record — including
            // MARK and dropped/reserved records — falling back to cycle_start
            // for a zero-event file. NOT max(trace_event.cycle).
            trunc_u64(summary.last_cycle),
            // the appended-row count (rows that actually landed), not `seq`
            event_count,
            // `bytes_written` = the .c64retrace file size, not the event region
            summary.bytes,
            overhead_ms,
            def.retention.as_str(),
            created_at.as_str(),
        ],
    )
    .map_err(|e| TraceReadError::duck_build("insert trace_run", e))?;

    if !marks.is_empty() {
        let mut stmt = conn
            .prepare("INSERT INTO trace_mark VALUES (?,?,?)")
            .map_err(|e| TraceReadError::duck_build("prepare trace_mark insert", e))?;
        for (cycle, label) in marks {
            stmt.execute(params![meta.run_id.as_str(), *cycle, label.as_str()])
                .map_err(|e| TraceReadError::duck_build("insert trace_mark", e))?;
        }
    }

    // 8 meta rows, in the reference order. `captured_at` is the INDEX-BUILD
    // wall time, not the capture time — same as the reference.
    let meta_rows: [(&str, &str); 8] = [
        ("schema_version", schema::SCHEMA_VERSION_726),
        ("writer_version", schema::WRITER_VERSION_726),
        ("run_id", meta.run_id.as_str()),
        ("source", "trace_event"),
        ("captured_at", created_at.as_str()),
        ("def_id", meta.def_id.as_str()),
        ("def_name", def.name.as_str()),
        ("retention", def.retention.as_str()),
    ];
    let mut stmt = conn
        .prepare("INSERT OR REPLACE INTO meta VALUES (?,?)")
        .map_err(|e| TraceReadError::duck_build("prepare meta insert", e))?;
    for (k, v) in meta_rows {
        stmt.execute(params![k, v])
            .map_err(|e| TraceReadError::duck_build("insert meta", e))?;
    }
    Ok(())
}

#[inline]
fn trunc_u64(v: f64) -> u64 {
    let t = v.trunc();
    if t <= 0.0 {
        0
    } else {
        t as u64
    }
}

/// `new Date().toISOString()` — UTC, milliseconds, trailing `Z`.
/// Hand-rolled (civil-from-days) so the crate needs no `chrono`.
pub fn now_iso8601() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let millis = d.subsec_millis();
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, dd) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Howard Hinnant's `civil_from_days` (days since 1970-01-01 → y/m/d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retrace_sibling_derivation() {
        assert_eq!(
            retrace_path_for(Path::new("/t/live_abc.duckdb")),
            PathBuf::from("/t/live_abc.c64retrace")
        );
        // No `.duckdb` suffix → append whole (reference behaviour).
        assert_eq!(
            retrace_path_for(Path::new("/t/store")),
            PathBuf::from("/t/store.c64retrace")
        );
    }

    #[test]
    fn iso8601_shape() {
        let s = now_iso8601();
        assert_eq!(s.len(), 24, "{s}");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
        // epoch day 0
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
    }
}
