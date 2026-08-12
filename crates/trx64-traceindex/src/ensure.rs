//! The index job registry + the lazy-on-read `ensure` family, and the `index`
//! op itself.
//!
//! # Honest summary of "incremental"
//!
//! The indexer is **not incremental and does not resume**. Every build is a
//! whole-file, all-or-nothing rebuild into a fresh temp store. What behaves
//! incrementally is (a) in-process memoization of in-flight builds — a second
//! caller for the same path joins the running job instead of starting a rival
//! builder (two builders renaming onto the same target would race) — and (b) an
//! existence check on the output path: **an existing `.duckdb` is trusted**.
//!
//! The optional staleness check (§6.7) is the one addition: it is a one-row
//! query, no clock skew, and it is **off by default** so the parity gate sees
//! byte-identical behaviour. `TRX64_INDEX_STALE_CHECK=1` turns it on.
//!
//! Builds run on a dedicated `std::thread` — the `duckdb` crate is blocking and
//! must never run on the tokio executor that serves the WS.

use crate::build::{self, retrace_path_for, IndexOverrides, IndexResult};
use crate::conn;
use crate::error::{Result, TraceReadError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug)]
enum JobState {
    Building,
    Done(IndexResult),
    Failed(String),
}

/// Handle to a running or settled index build. Obtain one from
/// [`start_background_index`]; clone it freely (`Arc` inside).
#[derive(Debug, Clone)]
pub struct IndexJob {
    inner: Arc<JobInner>,
}

#[derive(Debug)]
struct JobInner {
    state: Mutex<JobState>,
    settled: Condvar,
}

impl IndexJob {
    fn new() -> Self {
        IndexJob {
            inner: Arc::new(JobInner {
                state: Mutex::new(JobState::Building),
                settled: Condvar::new(),
            }),
        }
    }

    /// True while the build is still running.
    pub fn is_active(&self) -> bool {
        matches!(*self.inner.state.lock().unwrap(), JobState::Building)
    }

    /// Wait until settled, or until `bound` elapses. Returns `true` if settled.
    fn wait(&self, bound: Option<Duration>) -> bool {
        let mut st = self.inner.state.lock().unwrap();
        match bound {
            None => {
                while matches!(*st, JobState::Building) {
                    st = self.inner.settled.wait(st).unwrap();
                }
                true
            }
            Some(b) => {
                let deadline = Instant::now() + b;
                while matches!(*st, JobState::Building) {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return false;
                    }
                    let (g, t) = self.inner.settled.wait_timeout(st, left).unwrap();
                    st = g;
                    if t.timed_out() && matches!(*st, JobState::Building) {
                        return false;
                    }
                }
                true
            }
        }
    }

    fn settle(&self, state: JobState) {
        *self.inner.state.lock().unwrap() = state;
        self.inner.settled.notify_all();
    }

    fn failure(&self) -> Option<String> {
        match &*self.inner.state.lock().unwrap() {
            JobState::Failed(m) => Some(m.clone()),
            _ => None,
        }
    }

    /// Block until the build settles, then return its result.
    pub fn join(&self) -> Result<IndexResult> {
        self.wait(None);
        let st = self.inner.state.lock().unwrap();
        match &*st {
            JobState::Done(r) => Ok(r.clone()),
            JobState::Failed(m) => Err(TraceReadError::other(m.clone())),
            JobState::Building => unreachable!("wait(None) returned while still building"),
        }
    }
}

/// Settled jobs are KEPT (they are tiny, and the daemon is long-lived — the
/// reference's 30 s eviction is a Node-lifecycle artifact, not a contract).
/// `is_indexing` reads the job's own state, so it never lies.
fn registry() -> &'static Mutex<HashMap<PathBuf, IndexJob>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, IndexJob>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lookup(duckdb_path: &Path) -> Option<IndexJob> {
    registry().lock().unwrap().get(duckdb_path).cloned()
}

/// Kick a build on a worker thread. Never spawns a second builder for a path
/// that is already building — the running job is returned instead (two builders
/// renaming onto the same target would race).
pub fn start_background_index(
    retrace_path: &Path,
    duckdb_path: &Path,
    overrides: Option<IndexOverrides>,
) -> IndexJob {
    let mut reg = registry().lock().unwrap();
    if let Some(existing) = reg.get(duckdb_path) {
        if existing.is_active() {
            return existing.clone();
        }
    }
    let job = IndexJob::new();
    reg.insert(duckdb_path.to_path_buf(), job.clone());
    drop(reg);

    let retrace = retrace_path.to_path_buf();
    let duckdb = duckdb_path.to_path_buf();
    let j = job.clone();
    // A dedicated OS thread: the build is blocking + CPU-heavy and must never
    // occupy the async executor that serves the WS.
    let spawned = std::thread::Builder::new()
        .name("trx64-traceindex".into())
        .spawn(move || match build::index_binary_log(&retrace, &duckdb, overrides.as_ref()) {
            Ok(res) => j.settle(JobState::Done(res)),
            Err(e) => {
                let msg = e.to_string();
                eprintln!("[trace] background index failed for {}: {msg}", duckdb.display());
                j.settle(JobState::Failed(msg));
            }
        });
    if let Err(e) = spawned {
        job.settle(JobState::Failed(format!("could not spawn index thread: {e}")));
    }
    job
}

/// The last index-build failure for a path, if any.
pub fn index_error(duckdb_path: &Path) -> Option<String> {
    lookup(duckdb_path).and_then(|j| j.failure())
}

/// True while a background index for this path is in flight.
pub fn is_indexing(duckdb_path: &Path) -> bool {
    lookup(duckdb_path).map(|j| j.is_active()).unwrap_or(false)
}

/// Adaptive read-wait bound, scaled to the `.c64retrace` size.
///
/// `12 s base + 0.25 s/MB`, floored at 15 s (small traces stay snappy) and
/// capped at 150 s (under the MCP host's ~180 s per-tool stall limit — past the
/// cap the reader returns the retry-later error while the build finishes).
pub fn adaptive_index_timeout_ms(retrace_path: &Path) -> u64 {
    let mb = match std::fs::metadata(retrace_path) {
        Ok(m) => m.len() as f64 / (1024.0 * 1024.0),
        Err(_) => return 15_000, // no authority to size → conservative floor
    };
    let raw = (12_000.0 + mb * 250.0).round() as i64;
    raw.clamp(15_000, 150_000) as u64
}

/// Is the existing index stale relative to its authority?
///
/// Cheap + correct: `trace_run.bytes_written` already holds exactly the
/// `.c64retrace` size at build time, so one row answers it — no mtime, no clock
/// skew. Also compares `run_id` against the authority's header.
///
/// **Off by default** (`TRX64_INDEX_STALE_CHECK=1` enables it): the parity gate
/// must first confirm the default behaviour is unchanged. For a consistent
/// pair this check cannot alter any output.
pub fn index_is_stale(duckdb_path: &Path) -> bool {
    let retrace = retrace_path_for(duckdb_path);
    if !retrace.exists() || !duckdb_path.exists() {
        // A trace whose authority was deleted is still readable — never stale.
        return false;
    }
    let Ok(meta) = std::fs::metadata(&retrace) else { return false };
    let Ok(c) = conn::open_read_only(duckdb_path) else { return false };
    let row: std::result::Result<(Option<u64>, Option<String>), _> = c.query_row(
        "SELECT bytes_written, run_id FROM trace_run LIMIT 1",
        [],
        |r| Ok((r.get(0).ok(), r.get(1).ok())),
    );
    let Ok((bytes, run_id)) = row else { return true };
    if bytes != Some(meta.len()) {
        return true;
    }
    match (run_id, crate::decode::read_retrace_meta(&retrace)) {
        (Some(a), Ok(h)) => a != h.meta.run_id,
        _ => false,
    }
}

fn stale_check_enabled() -> bool {
    matches!(std::env::var("TRX64_INDEX_STALE_CHECK").as_deref(), Ok("1") | Ok("true"))
}

/// Lazy-on-read, UNBOUNDED: guarantee a queryable `.duckdb` exists for this
/// path, waiting for the build however long it takes.
pub fn ensure_index(duckdb_path: &Path) -> Result<()> {
    ensure_inner(duckdb_path, None)
}

/// Lazy-on-read, BOUNDED — the READ path's ensure.
///
/// Waits at most `timeout_ms` (default: [`adaptive_index_timeout_ms`]) and then
/// **errors** with the retry-later message while the build keeps running. The
/// unbounded variant would block for minutes on a multi-GB trace, tripping the
/// MCP host's stall limit and dropping the whole stdio connection.
pub fn ensure_index_bounded(duckdb_path: &Path, timeout_ms: Option<u64>) -> Result<()> {
    let retrace = retrace_path_for(duckdb_path);
    let bound = timeout_ms.unwrap_or_else(|| adaptive_index_timeout_ms(&retrace));
    ensure_inner(duckdb_path, Some(bound))
}

fn ensure_inner(duckdb_path: &Path, bound_ms: Option<u64>) -> Result<()> {
    ensure_inner_ov(duckdb_path, bound_ms, None)
}

/// As [`ensure_inner`], carrying the caller's [`IndexOverrides`] into the build it
/// may start. Spec 802 R2 J-3.
fn ensure_inner_ov(
    duckdb_path: &Path,
    bound_ms: Option<u64>,
    overrides: Option<IndexOverrides>,
) -> Result<()> {
    let retrace = retrace_path_for(duckdb_path);

    // 1. A build already in flight → wait for it.
    let job = match lookup(duckdb_path) {
        Some(j) if j.is_active() => Some(j),
        _ => {
            // 2. No store (or a stale one, when the check is enabled) and an
            //    authority present → kick a build.
            let needs_build =
                !duckdb_path.exists() || (stale_check_enabled() && index_is_stale(duckdb_path));
            if needs_build && retrace.exists() {
                Some(start_background_index(&retrace, duckdb_path, overrides.clone()))
            } else {
                None
            }
        }
    };

    if let Some(job) = job {
        match bound_ms {
            None => {
                job.wait(None);
            }
            Some(b) => {
                if !job.wait(Some(Duration::from_millis(b))) {
                    return Err(TraceReadError::IndexStillBuilding {
                        path: duckdb_path.display().to_string(),
                        waited_ms: b,
                    });
                }
            }
        }
    }

    if !duckdb_path.exists() {
        if let Some(why) = index_error(duckdb_path) {
            return Err(TraceReadError::IndexUnavailable {
                path: duckdb_path.display().to_string(),
                why,
            });
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// The `index` op
// ─────────────────────────────────────────────────────────────────────────────

/// `index` — explicitly decode a `.c64retrace` into its `.duckdb`, then report
/// how many events landed WITHOUT running an analysis query. Readers that open
/// the `.duckdb` directly (`trace_store_info`) never trigger the lazy build the
/// other ops get; this op closes that gap.
///
/// **Honesty about `bounded`** (the load-bearing part): the indexer streams the
/// whole file from byte 0 (the OLDEST event) to EOF. There is NO event cap and
/// NO newest/oldest truncation — a 1.2 GB trace's oldest events ARE indexed
/// (they go in first). `bounded` reflects only the WAIT, never dropped data.
pub fn op_index(
    duckdb_path: &Path,
    retrace_override: Option<&Path>,
    wait: bool,
) -> Result<serde_json::Value> {
    op_index_with(duckdb_path, retrace_override, wait, None)
}

/// As [`op_index`], plus the [`IndexOverrides`] the caller wants baked into the
/// `trace_run` header. Spec 802 R2 J-3: TRX64 writes no 0x01 Mark records, so a
/// finalize has to hand its in-memory marks to WHICHEVER index path runs —
/// background (`start_background_index`) or synchronous (here). Passing them on
/// only one path is how `trace_mark` came out empty while the stop reply showed
/// the marks correctly.
pub fn op_index_with(
    duckdb_path: &Path,
    retrace_override: Option<&Path>,
    wait: bool,
    overrides: Option<IndexOverrides>,
) -> Result<serde_json::Value> {
    let want_retrace = retrace_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| retrace_path_for(duckdb_path));
    if !duckdb_path.exists() && !want_retrace.exists() {
        return Err(TraceReadError::NoAuthority {
            duckdb: duckdb_path.display().to_string(),
            retrace: want_retrace.display().to_string(),
        });
    }

    let mut bounded_from = "none";
    if wait {
        ensure_inner_ov(duckdb_path, None, overrides.clone())?;
    } else if let Err(e) = ensure_inner_ov(duckdb_path, Some(adaptive_index_timeout_ms(&retrace_path_for(duckdb_path))), overrides.clone()) {
        match e {
            TraceReadError::IndexStillBuilding { .. } => {
                bounded_from = "wait-timeout";
                if !duckdb_path.exists() {
                    return Ok(serde_json::json!({
                        "duckdbPath": duckdb_path.display().to_string(),
                        "retracePath": want_retrace.display().to_string(),
                        "indexBuilt": false,
                        "bounded": true,
                        "boundedFrom": bounded_from,
                        "cap": serde_json::Value::Null,
                        "indexedFromOldest": true,
                        "eventsIndexed": serde_json::Value::Null,
                        "note": format!(
                            "index still building in the background (15s grace expired). The full \
                             .c64retrace is being decoded oldest→newest; NO events are dropped. \
                             Re-run `index` (or any read) to await completion. {e}"
                        ),
                    }));
                }
            }
            other => return Err(other),
        }
    }

    if !duckdb_path.exists() {
        return Err(TraceReadError::IndexProducedNoStore {
            path: duckdb_path.display().to_string(),
            why: index_error(duckdb_path),
        });
    }

    // Report what landed, so a caller can SEE the oldest event survived
    // (cycleRange.min == the trace's cycleStart ⇒ front intact).
    let (events_indexed, channels, range) = store_counts(duckdb_path)?;
    Ok(serde_json::json!({
        "duckdbPath": duckdb_path.display().to_string(),
        "retracePath": want_retrace.display().to_string(),
        "indexBuilt": true,
        "bounded": bounded_from != "none",
        "boundedFrom": bounded_from,
        "cap": serde_json::Value::Null,
        "indexedFromOldest": true,
        "eventsIndexed": events_indexed,
        "channels": channels,
        "cycleRange": range,
        "note": "Full streaming index (oldest→newest, no event cap). A 1.2 GB trace's oldest \
                 events are indexed (read first). bounded reflects only the wait, never dropped data.",
    }))
}

/// `events:total` + per-channel counts + the master-clock range, straight off
/// the finished store (the same numbers `getInfo`'s `tableCounts` /
/// `masterClockRange` carry).
fn store_counts(
    duckdb_path: &Path,
) -> Result<(u64, serde_json::Map<String, serde_json::Value>, serde_json::Value)> {
    let c = conn::open_read_only(duckdb_path)?;
    let mut channels = serde_json::Map::new();
    let mut total = 0u64;
    for row in conn::query_json(
        &c,
        "SELECT channel, count(*) FROM trace_event GROUP BY channel ORDER BY channel",
    )? {
        let name = row[0].as_str().unwrap_or_default().to_string();
        let n = row[1].as_u64().unwrap_or(0);
        total += n;
        channels.insert(name, serde_json::json!(n));
    }
    let range = conn::query_json(
        &c,
        "SELECT MIN(cycle), MAX(cycle) FROM trace_event WHERE cycle IS NOT NULL",
    )?;
    let range = match range.first() {
        Some(r) if !r[0].is_null() => serde_json::json!({ "min": r[0], "max": r[1] }),
        _ => serde_json::Value::Null,
    };
    Ok((total, channels, range))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_bound_floor_and_cap() {
        // Missing file → conservative floor.
        assert_eq!(adaptive_index_timeout_ms(Path::new("/nope/does-not-exist")), 15_000);
    }

    #[test]
    fn stale_check_is_off_by_default() {
        assert!(!stale_check_enabled(), "default must be the legacy behaviour");
    }

    #[test]
    fn missing_pair_reports_no_authority() {
        let e = op_index(Path::new("/nope/x.duckdb"), None, true).unwrap_err();
        assert!(
            e.to_string().starts_with("no trace store and no .c64retrace authority at /nope/x.duckdb"),
            "{e}"
        );
    }
}
