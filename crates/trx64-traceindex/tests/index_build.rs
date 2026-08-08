//! Integration proof for the native `.c64retrace` → DuckDB indexer (Spec 802).
//!
//! Two layers:
//!
//! * **Synthetic fixtures** built byte-exactly here (always run, deterministic):
//!   they cover every opcode, both format versions, the `seq`-gap property, the
//!   truncated-tail contract, the cross-window carry path, the atomic publish,
//!   and the full resulting schema (4 tables + 5 views).
//! * **A real trace** (`real_trace_indexes`): `.c64retrace` files are gitignored,
//!   so this test takes its input from `TRX64_TEST_RETRACE` or, failing that,
//!   the first trace it finds under the repo's own `tools/oracle/traces/` and
//!   `traces/` directories. With none present it prints a SKIP line and passes —
//!   it must never fail a clean clone.

use std::path::{Path, PathBuf};
use trx64_traceindex as ti;

// ── byte-exact fixture writer (mirrors trx64_trace::FrameSink) ───────────────

struct Fx {
    buf: Vec<u8>,
    version: u16,
}

impl Fx {
    fn new(meta_json: &str, version: u16) -> Self {
        let mut buf = Vec::new();
        buf.extend_from_slice(&ti::MAGIC[..]);
        buf.extend_from_slice(&version.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // flags
        buf.extend_from_slice(&(meta_json.len() as u32).to_le_bytes());
        buf.extend_from_slice(meta_json.as_bytes());
        Fx { buf, version }
    }
    fn cyc(&mut self, c: u64) {
        self.buf.extend_from_slice(&(c as f64).to_le_bytes());
    }
    fn cpu(&mut self, op: u8, c: u64, pc: u16) {
        self.buf.push(op);
        self.cyc(c);
        self.buf.extend_from_slice(&pc.to_le_bytes());
        self.buf.extend_from_slice(&[0xa9, 0x01, 0x02, 0x03, 0xfd, 0x24, 0x41, 0x00]);
    }
    fn mem(&mut self, op: u8, c: u64, addr: u16, val: u8, pc: u16, acc: u8, old: Option<u8>) {
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
    fn mark(&mut self, c: u64, label: &str) {
        self.buf.push(0x01);
        self.cyc(c);
        self.buf.extend_from_slice(&(label.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(label.as_bytes());
    }
    fn iec(&mut self, c: u64, lines: u16) {
        self.buf.push(0x23);
        self.cyc(c);
        self.buf.extend_from_slice(&lines.to_le_bytes());
    }
    fn vic(&mut self, c: u64, y: u16, kind: u8, v: u8) {
        self.buf.push(0x20);
        self.cyc(c);
        self.buf.extend_from_slice(&y.to_le_bytes());
        self.buf.push(kind);
        self.buf.push(v);
    }
    fn sid(&mut self, c: u64, reg: u16, v: u8) {
        self.buf.push(0x22);
        self.cyc(c);
        self.buf.extend_from_slice(&reg.to_le_bytes());
        self.buf.push(v);
    }
    fn reserved(&mut self, op: u8, c: u64) {
        self.buf.push(op);
        self.cyc(c);
        self.buf.extend_from_slice(&[0u8; 4]);
    }
    fn drive_head(&mut self, c: u64, ht: u8, sec: u8) {
        self.buf.push(0x34);
        self.cyc(c);
        self.buf.push(ht);
        self.buf.push(sec);
    }
    fn block_read(&mut self, c: u64, ht: u8, sec: u8, bytes: u16) {
        self.buf.push(0x35);
        self.cyc(c);
        self.buf.push(ht);
        self.buf.push(sec);
        self.buf.extend_from_slice(&bytes.to_le_bytes());
    }
}

const META: &str = r#"{"runId":"run_live-capture_test1","defId":"live-capture","defVersion":1,"defName":"live session capture","defJson":"{\"id\":\"live-capture\",\"version\":1,\"name\":\"live session capture\",\"domains\":[\"c64-cpu\",\"memory\",\"iec\"],\"retention\":\"evidence\"}","domains":["c64-cpu","memory","iec"],"cycleStart":1000,"createdAt":"2026-08-08T10:00:00.000Z"}"#;

/// A per-test scratch dir that cleans itself up.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!("trx64-traceindex-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rows(conn: &duckdb::Connection, sql: &str) -> Vec<Vec<serde_json::Value>> {
    ti::query_json(conn, sql).unwrap()
}

fn count(conn: &duckdb::Connection, sql: &str) -> u64 {
    rows(conn, sql)[0][0].as_u64().unwrap()
}

// ─────────────────────────────────────────────────────────────────────────────

/// The full-coverage build: every opcode family, marks, reserved records, the
/// loader-lens lanes, the resulting schema, and the seq-gap property.
#[test]
fn full_fixture_builds_the_complete_shape_b_store() {
    let sc = Scratch::new("full");
    let retrace = sc.join("live_full.c64retrace");
    let duckdb = sc.join("live_full.duckdb");

    let mut f = Fx::new(META, 2);
    // 2 C64 cpu rows
    f.cpu(0x10, 1000, 0xc000);
    f.cpu(0x10, 1002, 0xc002);
    // 1 drive cpu row
    f.cpu(0x30, 5000, 0xf556);
    // 3 mem rows: RAM write (with old), I/O-window write (no old), read
    f.mem(0x11, 1004, 0x0400, 0x41, 0xc002, 1, Some(0x20));
    f.mem(0x11, 1006, 0xd020, 0x06, 0xc005, 1, None);
    f.mem(0x11, 1008, 0x0400, 0x41, 0xc008, 0, None);
    // 1 drive mem row
    f.mem(0x31, 5002, 0x1c00, 0x10, 0xf556, 1, Some(0x00));
    // 1 IO_WRITE row (reserved opcode, but decodable → `io` channel)
    f.mem(0x12, 1010, 0xdc00, 0x7f, 0xc00b, 1, None);
    // 3 reserved (skip-only) INTERLEAVED here on purpose: their rows are
    // dropped but their seq numbers ARE consumed, so a real GAP appears in the
    // middle of trace_event.seq — the cheapest assertion that catches a
    // "helpfully fixed" seq counter (§I.7).
    f.reserved(0x21, 1011);
    f.reserved(0x32, 1011);
    f.reserved(0x33, 1011);
    // 1 iec, 1 vic, 1 sid
    f.iec(1012, 0b1_0000_0011);
    f.vic(1014, 51, 4, 0x1b);
    f.sid(1016, 0xd404, 0x21);
    // 2 marks (consume NO seq, land in trace_mark)
    f.mark(1100, "phase-boot");
    f.mark(1200, "phase-load");
    f.mark(1300, "phase-boot"); // repeat label → occurrence 2 in the anchors view
    // 2 loader-lens lanes: rows dropped, seq CONSUMED
    f.drive_head(5100, 70, 17);
    f.block_read(5200, 70, 17, 0x0154);

    std::fs::write(&retrace, &f.buf).unwrap();

    let res = ti::index_binary_log(&retrace, &duckdb, None).unwrap();

    // 11 rows land; 5 records are dropped but still consumed a seq; 3 marks
    // consumed none.
    assert_eq!(res.run_id, "run_live-capture_test1");
    assert_eq!(res.event_count, 11, "rows appended");
    assert_eq!(res.mark_count, 3);
    // channels that received rows: cpu, drive_pc, bus_access, io, iec, vic, sid
    assert_eq!(res.channels, 7);
    assert_eq!(res.output_path, duckdb.display().to_string());
    assert!(duckdb.exists(), "the store is published at the final path");
    assert!(
        std::fs::read_dir(&sc.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .all(|e| !e.file_name().to_string_lossy().contains(".tmp")),
        "no temp store left behind"
    );

    let conn = ti::open_read_only(&duckdb).unwrap();

    // ── row counts land in the tables ───────────────────────────────────────
    assert_eq!(count(&conn, "SELECT count(*) FROM trace_event"), 11);
    assert_eq!(count(&conn, "SELECT count(*) FROM trace_mark"), 3);
    assert_eq!(count(&conn, "SELECT count(*) FROM trace_run"), 1);
    assert_eq!(count(&conn, "SELECT count(*) FROM meta"), 8);

    // per-channel counts
    let ch: std::collections::HashMap<String, u64> = rows(
        &conn,
        "SELECT channel, count(*) FROM trace_event GROUP BY channel",
    )
    .into_iter()
    .map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_u64().unwrap()))
    .collect();
    assert_eq!(ch["cpu"], 2);
    assert_eq!(ch["drive_pc"], 1);
    assert_eq!(ch["bus_access"], 4, "3 c64 mem + 1 drive mem, routed by opcode");
    assert_eq!(ch["io"], 1, "only the 0x12 opcode routes to `io`");
    assert_eq!(ch["iec"], 1);
    assert_eq!(ch["vic"], 1);
    assert_eq!(ch["sid"], 1);

    // ── §I.7 the seq-gap property ──────────────────────────────────────────
    // 16 non-MARK records were decoded (11 rows + 3 reserved + 2 loader-lens);
    // MARK consumed none. The 3 reserved records sit in the MIDDLE, so seq has
    // a real 3-wide hole and MAX(seq)+1 > COUNT(*) by exactly the number of
    // dropped events that preceded the last row.
    let max_seq = count(&conn, "SELECT MAX(seq) FROM trace_event");
    let n_rows = count(&conn, "SELECT count(*) FROM trace_event");
    assert_eq!(n_rows, 11);
    assert_eq!(max_seq, 13, "3 reserved records consumed seq 8,9,10 before the iec row");
    assert_eq!(
        max_seq + 1 - n_rows,
        3,
        "exactly the dropped-record count that preceded the last row"
    );
    // the hole is where the reserved records were, not at the edges
    let present: Vec<u64> = rows(&conn, "SELECT seq FROM trace_event ORDER BY seq")
        .into_iter()
        .map(|r| r[0].as_u64().unwrap())
        .collect();
    assert_eq!(present, vec![0, 1, 2, 3, 4, 5, 6, 7, 11, 12, 13]);

    // ── trace_run header ───────────────────────────────────────────────────
    let r = &rows(
        &conn,
        "SELECT run_id, def_id, def_version, name, cycle_start, cycle_end, \
                event_count, bytes_written, retention, def_json, \
                start_checkpoint_id, stop_checkpoint_id, media_sha, media_name, \
                branch_id, overhead_ms \
         FROM trace_run",
    )[0];
    assert_eq!(r[0], serde_json::json!("run_live-capture_test1"));
    assert_eq!(r[1], serde_json::json!("live-capture"));
    assert_eq!(r[2], serde_json::json!(1));
    assert_eq!(r[3], serde_json::json!("live session capture"));
    assert_eq!(r[4], serde_json::json!(1000), "cycle_start from the header");
    assert_eq!(
        r[5],
        serde_json::json!(5200),
        "cycle_end = LAST DECODED record incl. dropped lanes, not max(trace_event.cycle)"
    );
    assert_eq!(r[6], serde_json::json!(11));
    assert_eq!(
        r[7],
        serde_json::json!(f.buf.len()),
        "bytes_written = .c64retrace size"
    );
    assert_eq!(r[8], serde_json::json!("evidence"));
    // def_json is stored VERBATIM (R2 J-2), not re-serialized.
    let hdr = ti::read_retrace_meta(&retrace).unwrap();
    assert_eq!(r[9], serde_json::json!(hdr.meta.def_json));
    for i in 10..16 {
        assert!(r[i].is_null(), "column {i} has no source in the header");
    }

    // ── meta rows ──────────────────────────────────────────────────────────
    let m: std::collections::HashMap<String, String> =
        rows(&conn, "SELECT key, value FROM meta ORDER BY key")
            .into_iter()
            .map(|r| {
                (
                    r[0].as_str().unwrap().to_string(),
                    r[1].as_str().unwrap().to_string(),
                )
            })
            .collect();
    assert_eq!(m["schema_version"], "spec-708-streaming");
    assert_eq!(m["writer_version"], "spec-726.2c");
    assert_eq!(m["source"], "trace_event");
    assert_eq!(m["run_id"], "run_live-capture_test1");
    assert_eq!(m["def_id"], "live-capture");
    assert_eq!(m["def_name"], "live session capture");
    assert_eq!(m["retention"], "evidence");
    assert!(m["captured_at"].ends_with('Z'), "{}", m["captured_at"]);

    // ── data_json is byte-exact, key order included ─────────────────────────
    let dj = rows(
        &conn,
        "SELECT data_json FROM trace_event WHERE channel='cpu' ORDER BY seq LIMIT 1",
    );
    assert_eq!(
        dj[0][0],
        serde_json::json!(
            r#"{"pc":49152,"opcode":169,"b1":65,"b2":0,"a":1,"x":2,"y":3,"sp":253,"p":36}"#
        )
    );
    let dj = rows(
        &conn,
        "SELECT data_json FROM trace_event WHERE channel='drive_pc'",
    );
    assert_eq!(
        dj[0][0],
        serde_json::json!(
            r#"{"pc":62806,"opcode":169,"b1":65,"b2":0,"a":1,"x":2,"y":3,"sp":253,"p":36,"side":"drive","clk":5000}"#
        ),
        "J-1: clk is an INTEGER, never 5000.0"
    );

    // ── the 4 tables + 5 views exist, and the views answer ──────────────────
    let tables: Vec<String> = rows(
        &conn,
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema='main' ORDER BY table_name",
    )
    .into_iter()
    .map(|r| r[0].as_str().unwrap().to_string())
    .collect();
    for want in [
        "trace_run",
        "trace_event",
        "trace_mark",
        "meta",
        "instructions",
        "bus_events",
        "chip_events",
        "anchors",
        "rollups",
    ] {
        assert!(tables.contains(&want.to_string()), "missing {want} in {tables:?}");
    }
    assert_eq!(tables.len(), 9, "exactly 4 tables + 5 views: {tables:?}");

    assert_eq!(
        count(&conn, "SELECT count(*) FROM instructions"),
        3,
        "2 c64 + 1 drive"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM instructions WHERE cpu='drive8'"),
        1
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM bus_events"),
        6,
        "bus_access + io + iec"
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM chip_events"), 0);
    assert_eq!(count(&conn, "SELECT count(*) FROM rollups"), 0);
    assert_eq!(count(&conn, "SELECT count(*) FROM anchors"), 3);

    // anchors: per-label occurrence ordinal in cycle order
    let a = rows(
        &conn,
        "SELECT name, occurrence, clock, cpu, pc, source FROM anchors ORDER BY name, occurrence",
    );
    assert_eq!(a[0][0], serde_json::json!("phase-boot"));
    assert_eq!(a[0][1], serde_json::json!(1));
    assert_eq!(a[0][2], serde_json::json!(1100));
    assert_eq!(a[1][1], serde_json::json!(2), "repeat label → occurrence 2");
    assert_eq!(a[1][2], serde_json::json!(1300));
    assert!(a[0][3].is_null() && a[0][4].is_null(), "cpu/pc are typed NULLs");
    assert_eq!(a[0][5], serde_json::json!("trace_mark"));

    // the bus_events view resolves the Spec-753 mutation surface
    let ov = rows(
        &conn,
        "SELECT addr, kind, value, old_value FROM bus_events \
         WHERE cpu='c64' AND addr=1024 ORDER BY seq",
    );
    assert_eq!(ov[0][1], serde_json::json!("write"));
    assert_eq!(ov[0][3], serde_json::json!(32), "oldValue present");
    assert_eq!(ov[1][1], serde_json::json!("read"));
    assert!(ov[1][3].is_null(), "missing key ⇒ SQL NULL, not 0");

    // iec lanes surface as booleans
    let iec = rows(
        &conn,
        "SELECT kind, line_atn, line_clk, line_data FROM bus_events WHERE kind='line_change'",
    );
    assert_eq!(iec[0][1], serde_json::json!(true));
    assert_eq!(iec[0][2], serde_json::json!(true));
    assert_eq!(iec[0][3], serde_json::json!(false));
}

/// A v1 trace (mem records one byte shorter) indexes without misalignment.
#[test]
fn v1_trace_indexes_at_the_right_record_width() {
    let sc = Scratch::new("v1");
    let retrace = sc.join("v1.c64retrace");
    let duckdb = sc.join("v1.duckdb");
    let mut f = Fx::new(META, 1);
    for i in 0..50u64 {
        f.mem(0x11, 1000 + i, 0x0400 + i as u16, i as u8, 0xc000, 1, None);
        f.cpu(0x10, 1000 + i, 0xc000);
    }
    std::fs::write(&retrace, &f.buf).unwrap();

    let res = ti::index_binary_log(&retrace, &duckdb, None).unwrap();
    assert_eq!(res.event_count, 100, "misalignment would produce garbage/errors");
    let conn = ti::open_read_only(&duckdb).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM trace_event WHERE channel='cpu'"), 50);
    // v1 has no has_old bit → no oldValue key anywhere.
    assert_eq!(
        count(&conn, "SELECT count(*) FROM bus_events WHERE old_value IS NOT NULL"),
        0
    );
}

/// A trace killed mid-write still indexes: the truncated final record is
/// dropped silently, everything before it lands.
#[test]
fn truncated_trace_indexes_up_to_the_last_complete_record() {
    let sc = Scratch::new("trunc");
    let mut f = Fx::new(META, 2);
    for i in 0..200u64 {
        f.cpu(0x10, 1000 + i, 0xc000 + i as u16);
    }
    let whole = f.buf.clone();

    for cut in [1usize, 7, 18] {
        let retrace = sc.join(&format!("cut{cut}.c64retrace"));
        let duckdb = sc.join(&format!("cut{cut}.duckdb"));
        std::fs::write(&retrace, &whole[..whole.len() - cut]).unwrap();
        let res = ti::index_binary_log(&retrace, &duckdb, None).unwrap();
        assert_eq!(res.event_count, 199, "cut={cut}: last partial record dropped");
        let conn = ti::open_read_only(&duckdb).unwrap();
        assert_eq!(count(&conn, "SELECT count(*) FROM trace_event"), 199);
        assert_eq!(
            rows(&conn, "SELECT cycle_end FROM trace_run")[0][0],
            serde_json::json!(1198)
        );
    }
}

/// A header-only file is valid: zero events, `cycle_end == cycle_start`.
#[test]
fn zero_event_trace_is_valid() {
    let sc = Scratch::new("zero");
    let retrace = sc.join("empty.c64retrace");
    let duckdb = sc.join("empty.duckdb");
    std::fs::write(&retrace, &Fx::new(META, 2).buf).unwrap();

    let res = ti::index_binary_log(&retrace, &duckdb, None).unwrap();
    assert_eq!(res.event_count, 0);
    assert_eq!(res.mark_count, 0);
    assert_eq!(res.channels, 0);
    let conn = ti::open_read_only(&duckdb).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM trace_event"), 0);
    let r = &rows(&conn, "SELECT cycle_start, cycle_end FROM trace_run")[0];
    assert_eq!(r[0], serde_json::json!(1000));
    assert_eq!(r[1], serde_json::json!(1000), "falls back to cycleStart");
}

/// Forcing a tiny read window exercises the cross-boundary carry path — the
/// only way to test it without a multi-GB fixture. The result must be identical
/// to the default-window build.
#[test]
fn tiny_window_carry_path_matches_the_default_build() {
    let sc = Scratch::new("window");
    let retrace = sc.join("w.c64retrace");
    let mut f = Fx::new(META, 2);
    for i in 0..500u64 {
        f.cpu(0x10, 1000 + i, 0xc000);
        f.mem(0x11, 1000 + i, 0x0400, i as u8, 0xc000, 1, Some(0));
        if i % 97 == 0 {
            f.mark(1000 + i, "tick");
        }
    }
    std::fs::write(&retrace, &f.buf).unwrap();

    let a = ti::index_binary_log(&retrace, &sc.join("a.duckdb"), None).unwrap();

    // Env is process-global; this test owns it (the others do not read it).
    std::env::set_var("TRX64_INDEX_HEADER_BYTES", "4096");
    std::env::set_var("TRX64_INDEX_WINDOW_BYTES", "65536");
    let b = ti::index_binary_log(&retrace, &sc.join("b.duckdb"), None).unwrap();
    std::env::remove_var("TRX64_INDEX_HEADER_BYTES");
    std::env::remove_var("TRX64_INDEX_WINDOW_BYTES");

    assert_eq!(a.event_count, 1000);
    assert_eq!(a.event_count, b.event_count, "carry path must not drop or duplicate");
    assert_eq!(a.mark_count, b.mark_count);
    assert_eq!(a.channels, b.channels);

    let ca = ti::open_read_only(&sc.join("a.duckdb")).unwrap();
    let cb = ti::open_read_only(&sc.join("b.duckdb")).unwrap();
    let q = "SELECT seq, cycle, channel, data_json FROM trace_event ORDER BY seq";
    assert_eq!(rows(&ca, q), rows(&cb, q), "row-for-row identical");
}

/// Opcode 0x40 is unskippable: the whole build fails, and nothing is published.
#[test]
fn media_write_opcode_fails_the_build_and_publishes_nothing() {
    let sc = Scratch::new("op40");
    let retrace = sc.join("bad.c64retrace");
    let duckdb = sc.join("bad.duckdb");
    let mut f = Fx::new(META, 2);
    f.cpu(0x10, 1000, 0xc000);
    f.buf.push(0x40);
    f.buf.extend_from_slice(&[0u8; 64]);
    std::fs::write(&retrace, &f.buf).unwrap();

    let e = ti::index_binary_log(&retrace, &duckdb, None).unwrap_err();
    assert!(e.to_string().starts_with("c64retrace: cannot skip opcode 0x40 at "), "{e}");
    assert!(!duckdb.exists(), "a failed build is never published");
    let leftovers: Vec<_> = std::fs::read_dir(&sc.0)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp store not cleaned up: {leftovers:?}");
}

/// Bad magic / unsupported version fail before any store is created.
#[test]
fn header_errors_fail_before_creating_a_store() {
    let sc = Scratch::new("hdr");
    let duckdb = sc.join("x.duckdb");

    let bad = sc.join("badmagic.c64retrace");
    let mut b = Fx::new(META, 2).buf;
    b[0] = b'X';
    std::fs::write(&bad, &b).unwrap();
    assert_eq!(
        ti::index_binary_log(&bad, &duckdb, None).unwrap_err().to_string(),
        "c64retrace: bad magic"
    );
    assert!(!duckdb.exists());

    let v9 = sc.join("v9.c64retrace");
    std::fs::write(&v9, &Fx::new(META, 9).buf).unwrap();
    assert_eq!(
        ti::index_binary_log(&v9, &duckdb, None).unwrap_err().to_string(),
        "c64retrace: unsupported format version 9 (this build reads v1..v2)"
    );
    assert!(!duckdb.exists());
}

/// Overrides supply the fields the header cannot know — including `marks`,
/// which is how a TRX64-captured trace gets a populated `trace_mark` /
/// `anchors` at all (Spec 802 R2 J-3: TRX64 writes no 0x01 frames).
#[test]
fn overrides_supply_marks_and_stop_time_fields() {
    let sc = Scratch::new("ovr");
    let retrace = sc.join("o.c64retrace");
    let duckdb = sc.join("o.duckdb");
    let mut f = Fx::new(META, 2);
    f.cpu(0x10, 1000, 0xc000);
    std::fs::write(&retrace, &f.buf).unwrap();

    let ov = ti::IndexOverrides {
        stop_checkpoint_id: Some("ckpt_42".into()),
        branch_id: Some("branch_a".into()),
        overhead_ms: Some(12.5),
        marks: Some(vec![(1000, "start".into()), (2000, "stop".into())]),
    };
    let res = ti::index_binary_log(&retrace, &duckdb, Some(&ov)).unwrap();
    assert_eq!(res.mark_count, 2);

    let conn = ti::open_read_only(&duckdb).unwrap();
    let r = &rows(
        &conn,
        "SELECT stop_checkpoint_id, branch_id, overhead_ms FROM trace_run",
    )[0];
    assert_eq!(r[0], serde_json::json!("ckpt_42"));
    assert_eq!(r[1], serde_json::json!("branch_a"));
    assert_eq!(r[2], serde_json::json!(12.5));
    assert_eq!(count(&conn, "SELECT count(*) FROM anchors"), 2);
}

/// `op_index` (the `index` op) reports honestly, and an existing store is
/// trusted rather than rebuilt.
#[test]
fn op_index_reports_counts_and_trusts_an_existing_store() {
    let sc = Scratch::new("op");
    let retrace = sc.join("i.c64retrace");
    let duckdb = sc.join("i.duckdb");
    let mut f = Fx::new(META, 2);
    for i in 0..10u64 {
        f.cpu(0x10, 1000 + i, 0xc000);
    }
    std::fs::write(&retrace, &f.buf).unwrap();

    let out = ti::op_index(&duckdb, None, true).unwrap();
    assert_eq!(out["indexBuilt"], serde_json::json!(true));
    assert_eq!(out["bounded"], serde_json::json!(false));
    assert_eq!(out["boundedFrom"], serde_json::json!("none"));
    assert!(out["cap"].is_null(), "there is no event cap");
    assert_eq!(out["indexedFromOldest"], serde_json::json!(true));
    assert_eq!(out["eventsIndexed"], serde_json::json!(10));
    assert_eq!(out["channels"]["cpu"], serde_json::json!(10));
    assert_eq!(out["cycleRange"]["min"], serde_json::json!(1000));
    assert_eq!(out["cycleRange"]["max"], serde_json::json!(1009));

    // An existing .duckdb is trusted unconditionally (legacy default): rewriting
    // the authority does NOT trigger a rebuild while the stale check is off.
    let before = std::fs::metadata(&duckdb).unwrap().len();
    let mut f2 = Fx::new(META, 2);
    for i in 0..500u64 {
        f2.cpu(0x10, 9000 + i, 0xd000);
    }
    std::fs::write(&retrace, &f2.buf).unwrap();
    let out2 = ti::op_index(&duckdb, None, true).unwrap();
    assert_eq!(out2["eventsIndexed"], serde_json::json!(10), "not re-indexed");
    assert_eq!(std::fs::metadata(&duckdb).unwrap().len(), before);

    // …but the (opt-in) staleness check SEES it: bytes_written no longer matches.
    assert!(ti::index_is_stale(&duckdb), "size mismatch ⇒ stale");
}

/// `ensure_index` is idempotent and never spawns rival builders for one path.
#[test]
fn ensure_index_is_idempotent_and_single_builder() {
    let sc = Scratch::new("ensure");
    let retrace = sc.join("e.c64retrace");
    let duckdb = sc.join("e.duckdb");
    let mut f = Fx::new(META, 2);
    for i in 0..100u64 {
        f.cpu(0x10, 1000 + i, 0xc000);
    }
    std::fs::write(&retrace, &f.buf).unwrap();

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let d = duckdb.clone();
            std::thread::spawn(move || ti::ensure_index(&d))
        })
        .collect();
    for h in handles {
        h.join().unwrap().unwrap();
    }
    assert!(duckdb.exists());
    assert!(!ti::is_indexing(&duckdb));
    assert!(ti::index_error(&duckdb).is_none());
    let conn = ti::open_read_only(&duckdb).unwrap();
    assert_eq!(
        count(&conn, "SELECT count(*) FROM trace_event"),
        100,
        "exactly one builder wrote the store"
    );

    // A second ensure on a present store is a no-op.
    ti::ensure_index(&duckdb).unwrap();
    ti::ensure_index_bounded(&duckdb, Some(1)).unwrap();
}

/// `with_conn` — the shared stage-2 entry point — builds the index lazily on
/// first read and hands back a working connection.
#[test]
fn with_conn_builds_lazily_on_first_read() {
    let sc = Scratch::new("withconn");
    let retrace = sc.join("w2.c64retrace");
    let duckdb = sc.join("w2.duckdb");
    let mut f = Fx::new(META, 2);
    f.cpu(0x10, 1000, 0xc000);
    f.mark(1001, "m");
    std::fs::write(&retrace, &f.buf).unwrap();
    assert!(!duckdb.exists());

    let n = ti::with_conn(&duckdb, |conn, shape| {
        assert_eq!(shape, ti::StoreShape::Spec726);
        Ok(count(conn, "SELECT count(*) FROM trace_event"))
    })
    .unwrap();
    assert_eq!(n, 1);
    assert!(duckdb.exists(), "lazy-on-read materialized the index");
}

// ─────────────────────────────────────────────────────────────────────────────
// Real trace
// ─────────────────────────────────────────────────────────────────────────────

/// Locate a real `.c64retrace`. They are gitignored, so this is best-effort:
/// `TRX64_TEST_RETRACE` wins, else the first file under the repo's own trace
/// directories.
fn find_real_trace() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("TRX64_TEST_RETRACE") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?;
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in ["tools/oracle/traces", "traces"] {
        let d = repo.join(dir);
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "c64retrace").unwrap_or(false) {
                found.push(p);
            }
        }
    }
    // Smallest first — the point is to prove the reader, not to burn minutes.
    found.sort_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(u64::MAX));
    found.into_iter().next()
}

#[test]
fn real_trace_indexes() {
    let Some(retrace) = find_real_trace() else {
        eprintln!(
            "SKIP real_trace_indexes: no .c64retrace found (they are gitignored). \
             Set TRX64_TEST_RETRACE=/path/to/x.c64retrace to run it."
        );
        return;
    };
    let size = std::fs::metadata(&retrace).unwrap().len();
    let sc = Scratch::new("real");
    let duckdb = sc.join("real.duckdb");

    let hdr = ti::read_retrace_meta(&retrace).unwrap();
    eprintln!(
        "real trace: {} ({} bytes, v{}, runId={}, defId={}, domains={:?})",
        retrace.display(),
        size,
        hdr.version,
        hdr.meta.run_id,
        hdr.meta.def_id,
        hdr.meta.domains
    );

    let t0 = std::time::Instant::now();
    let res = ti::index_binary_log(&retrace, &duckdb, None).unwrap();
    let dt = t0.elapsed();
    eprintln!(
        "indexed {} events / {} marks / {} channels in {:.2}s ({:.1} MB/s)",
        res.event_count,
        res.mark_count,
        res.channels,
        dt.as_secs_f64(),
        (size as f64 / (1024.0 * 1024.0)) / dt.as_secs_f64().max(1e-9),
    );

    assert_eq!(res.run_id, hdr.meta.run_id);
    assert!(res.event_count > 0, "a real capture must produce rows");
    assert!(res.channels > 0);

    let conn = ti::open_read_only(&duckdb).unwrap();

    // Row counts landed in the tables.
    assert_eq!(count(&conn, "SELECT count(*) FROM trace_event"), res.event_count);
    assert_eq!(count(&conn, "SELECT count(*) FROM trace_mark"), res.mark_count);
    assert_eq!(count(&conn, "SELECT count(*) FROM trace_run"), 1);
    assert_eq!(count(&conn, "SELECT count(*) FROM meta"), 8);

    for r in rows(&conn, "SELECT channel, count(*) FROM trace_event GROUP BY channel ORDER BY 2 DESC")
    {
        eprintln!("  channel {:<12} {}", r[0].as_str().unwrap(), r[1]);
    }

    // The header agrees with the file.
    let run = &rows(
        &conn,
        "SELECT run_id, bytes_written, cycle_start, cycle_end, event_count FROM trace_run",
    )[0];
    assert_eq!(run[0], serde_json::json!(hdr.meta.run_id));
    assert_eq!(run[1], serde_json::json!(size), "bytes_written = file size");
    assert_eq!(run[4], serde_json::json!(res.event_count));
    assert!(
        run[3].as_u64().unwrap() >= run[2].as_u64().unwrap(),
        "cycle_end >= cycle_start"
    );

    // The compat views answer over real data.
    let instr = count(&conn, "SELECT count(*) FROM instructions");
    let bus = count(&conn, "SELECT count(*) FROM bus_events");
    eprintln!("  view instructions={instr} bus_events={bus} anchors={}",
        count(&conn, "SELECT count(*) FROM anchors"));
    assert_eq!(
        instr + bus,
        count(
            &conn,
            "SELECT count(*) FROM trace_event \
             WHERE channel IN ('cpu','drive_pc','bus_access','io','iec')"
        ),
        "every cpu/mem/iec row is reachable through exactly one view"
    );

    // Every data_json is valid JSON with no float-formatted cycle (J-1).
    for r in rows(
        &conn,
        "SELECT data_json FROM trace_event ORDER BY seq LIMIT 500",
    ) {
        let s = r[0].as_str().unwrap();
        serde_json::from_str::<serde_json::Value>(s).expect("data_json parses");
        assert!(!s.contains(".0,") && !s.contains(".0}"), "float-formatted number: {s}");
    }

    // seq is dense-or-gapped but never duplicated, and starts at 0.
    assert_eq!(count(&conn, "SELECT count(DISTINCT seq) FROM trace_event"), res.event_count);
    assert_eq!(
        rows(&conn, "SELECT MIN(seq) FROM trace_event")[0][0],
        serde_json::json!(0),
        "the FIRST record of the file is seq 0 — oldest events are indexed first"
    );
}
