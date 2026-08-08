//! The `map` monitor verb — memory-map TEXT renderer (Spec 802 §4.1, op `map`).
//!
//! Native port of C64RE's `src/server-tools/trace-memory-map.ts` (Spec 753 P3).
//! It is **formatting over query results**: two SQL statements against the
//! store's `bus_events` / `instructions` (base tables on a Shape-A store, the
//! persisted compat views on a Shape-B one — so the same SQL serves both and
//! this renderer never branches on [`StoreShape`](crate::schema::StoreShape)),
//! a 256-page model, and a fixed layout.
//!
//! **BEHAVIOUR capture, NOT grounding** (Spec 752 §6): the map answers "what
//! runs/writes where, what is free AT RUNTIME on THIS path". It never claims
//! "what a block IS" — that stays the extracted bytes + disasm. The rendered
//! text carries a mandatory coverage banner so a hole is never mistaken for a
//! proof; the banner is part of the output contract, not decoration.
//!
//! # Parity
//!
//! Monitor transcripts pin this output, so the port is byte-for-byte:
//!
//! * the two SQL strings are reproduced character-for-character (C64RE's own
//!   `trace_memory_map` tool builds the *same* strings and routes them through
//!   `store_fn safeQuery`, so both paths must keep agreeing);
//! * every literal, every column width, both-spaces-between-fields, the
//!   TAB-separated region table, and the non-ASCII characters `—` (U+2014),
//!   `⚠` (U+26A0) and `≠` (U+2260) are verbatim;
//! * the empty case returns [`MAP_EMPTY_TEXT`] verbatim — the sidecar's
//!   `r?.text ?? "map: empty (…)"` fallback.
//!
//! # Entry points
//!
//! | fn | use |
//! |---|---|
//! | [`render_map`] | the monitor verb: `map [c64\|drive8]` |
//! | [`render_map_with`] | same + `static_ranges` / `run_label` (the C64RE tool path) |
//! | [`op_map`] | the WS/daemon `map` op → `{"text": …}` |
//! | [`build_memory_map_text`] | renderer over an injected query runner (tests, gates) |
//! | [`build_memory_map`] / [`render_memory_map`] | the pure model + layout |

use crate::conn::{query_json, with_conn};
use crate::error::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Returned by the `map` op when the aggregate query yields no rows — i.e. the
/// trace captured no memory accesses at all (no mem-row domain enabled).
///
/// The dash is U+2014. Monitor transcripts depend on this string verbatim.
pub const MAP_EMPTY_TEXT: &str =
    "map: empty (the trace captured no memory accesses — enable the memory domain)";

// ─────────────────────────────────────────────────────────────────────────────
// Model
// ─────────────────────────────────────────────────────────────────────────────

/// One row of the aggregate query: per-page write/read/mutation counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemMapPageRow {
    pub page: i64,
    pub writes: i64,
    pub reads: i64,
    pub mutations: i64,
    pub first_clk: i64,
    pub last_clk: i64,
    pub writer_pcs: i64,
}

/// A statically known owner of an address range (module load-map / analysis
/// JSON). Never supplied by the monitor `map` verb — only by the C64RE tool
/// path, which can reconcile the trace against static knowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemMapStaticRange {
    pub from: i64,
    pub to: i64,
    pub label: Option<String>,
}

/// What a page did during the traced run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemRole {
    Code,
    CodeWrite,
    DataW,
    DataWMut,
    DataR,
    Untouched,
}

impl MemRole {
    /// The role string as it appears in the `regions` table.
    pub fn as_str(self) -> &'static str {
        match self {
            MemRole::Code => "code",
            MemRole::CodeWrite => "code-write",
            MemRole::DataW => "data-w",
            MemRole::DataWMut => "data-w-mut",
            MemRole::DataR => "data-r",
            MemRole::Untouched => "untouched",
        }
    }

    /// The grid cell character (`#` for static-owned-untouched is decided by
    /// [`page_char`], which needs the page's static flag as well).
    fn role_char(self) -> char {
        match self {
            MemRole::Code => 'C',
            MemRole::CodeWrite => 'c',
            MemRole::DataW => 'W',
            MemRole::DataWMut => 'M',
            MemRole::DataR => 'R',
            MemRole::Untouched => '.',
        }
    }
}

impl std::fmt::Display for MemRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemMapPage {
    pub page: usize,
    pub role: MemRole,
    pub writes: i64,
    pub reads: i64,
    pub mutations: i64,
    pub writer_pcs: i64,
    pub first_clk: i64,
    pub last_clk: i64,
    pub static_occupied: bool,
    pub static_label: Option<String>,
    pub provably_free: bool,
    pub ef_legal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemMapRegion {
    pub from_page: usize,
    pub to_page: usize,
    pub role: MemRole,
    pub writes: i64,
    pub reads: i64,
    pub mutations: i64,
    pub writer_pcs: i64,
    pub static_label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemMapFreeHole {
    pub from_page: usize,
    pub to_page: usize,
    pub pages: usize,
    pub ef_legal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemMapTotals {
    pub code_pages: usize,
    pub written_pages: usize,
    pub read_pages: usize,
    pub untouched_pages: usize,
    pub mutated_pages: usize,
    pub free_pages: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemMapResult {
    pub cpu: String,
    /// Always 256 entries, page 0..=255.
    pub pages: Vec<MemMapPage>,
    pub regions: Vec<MemMapRegion>,
    pub free_holes: Vec<MemMapFreeHole>,
    /// Static-owned but untouched this run — **not** provably free.
    pub static_untouched: Vec<MemMapPage>,
    pub totals: MemMapTotals,
}

/// Options for the renderer. `cpu` defaults to `"c64"` (the TS `opts.cpu ?? "c64"`).
#[derive(Debug, Clone, Default)]
pub struct MapOptions {
    pub cpu: Option<String>,
    /// Only the C64RE tool path supplies these; via the monitor `map` verb the
    /// list is always empty, so `#` never appears in the grid, the `static`
    /// column is always `-`, and the "static-owned but UNTOUCHED" block never
    /// renders.
    pub static_ranges: Vec<MemMapStaticRange>,
    pub run_label: Option<String>,
}

impl MapOptions {
    pub fn for_cpu(cpu: &str) -> Self {
        MapOptions { cpu: Some(cpu.to_string()), ..Default::default() }
    }
    fn cpu_or_default(&self) -> &str {
        self.cpu.as_deref().unwrap_or("c64")
    }
}

/// EF-legal RAM: `$0000-$7FFF` or `$C000-$CFFF` — a resident EAPI / relocated
/// fastloader / save-overlay cache may only live there on an EasyFlash.
pub fn is_ef_legal_page(page: usize) -> bool {
    page < 0x80 || (0xc0..=0xcf).contains(&page)
}

fn role_of(code: bool, writes: i64, reads: i64, mutations: i64) -> MemRole {
    if code && writes > 0 {
        return MemRole::CodeWrite;
    }
    if code {
        return MemRole::Code;
    }
    if writes > 0 {
        return if mutations > 0 { MemRole::DataWMut } else { MemRole::DataW };
    }
    if reads > 0 {
        return MemRole::DataR;
    }
    MemRole::Untouched
}

/// Build the 256-page model, its regions, free holes and totals.
pub fn build_memory_map(
    cpu: &str,
    page_rows: &[MemMapPageRow],
    code_pages: &HashSet<usize>,
    static_ranges: &[MemMapStaticRange],
) -> MemMapResult {
    // `byPage.set(r.page & 0xff, r)` — last row for a page wins.
    let mut by_page: HashMap<usize, MemMapPageRow> = HashMap::new();
    for r in page_rows {
        by_page.insert((r.page & 0xff) as usize, *r);
    }
    // `statics.find(...)` — FIRST overlapping range wins, input order.
    let static_at = |page: usize| -> Option<&MemMapStaticRange> {
        let lo = (page as i64) << 8;
        let hi = lo | 0xff;
        static_ranges.iter().find(|s| s.from <= hi && s.to >= lo)
    };

    let mut pages: Vec<MemMapPage> = Vec::with_capacity(256);
    for p in 0..256usize {
        let r = by_page.get(&p);
        let writes = r.map(|r| r.writes).unwrap_or(0);
        let reads = r.map(|r| r.reads).unwrap_or(0);
        let mutations = r.map(|r| r.mutations).unwrap_or(0);
        let code = code_pages.contains(&p);
        let role = role_of(code, writes, reads, mutations);
        let st = static_at(p);
        let untouched = role == MemRole::Untouched;
        pages.push(MemMapPage {
            page: p,
            role,
            writes,
            reads,
            mutations,
            writer_pcs: r.map(|r| r.writer_pcs).unwrap_or(0),
            first_clk: r.map(|r| r.first_clk).unwrap_or(0),
            last_clk: r.map(|r| r.last_clk).unwrap_or(0),
            static_occupied: st.is_some(),
            static_label: st.and_then(|s| s.label.clone()),
            provably_free: untouched && st.is_none(),
            ef_legal: is_ef_legal_page(p),
        });
    }

    // Contiguous runs of same role AND same static owner → regions (split on the
    // static boundary so the `static` column names only the pages it really owns).
    let mut regions: Vec<MemMapRegion> = Vec::new();
    let mut p = 0usize;
    while p < 256 {
        let role = pages[p].role;
        let label = pages[p].static_label.clone();
        let mut q = p;
        let (mut writes, mut reads, mut mutations, mut writer_pcs) = (0i64, 0i64, 0i64, 0i64);
        while q < 256 && pages[q].role == role && pages[q].static_label == label {
            writes += pages[q].writes;
            reads += pages[q].reads;
            mutations += pages[q].mutations;
            writer_pcs = writer_pcs.max(pages[q].writer_pcs);
            q += 1;
        }
        regions.push(MemMapRegion {
            from_page: p,
            to_page: q - 1,
            role,
            writes,
            reads,
            mutations,
            writer_pcs,
            static_label: label,
        });
        p = q;
    }

    // Provably-free contiguous runs → free holes.
    let mut free_holes: Vec<MemMapFreeHole> = Vec::new();
    let mut p = 0usize;
    while p < 256 {
        if !pages[p].provably_free {
            p += 1;
            continue;
        }
        let mut q = p;
        while q < 256 && pages[q].provably_free {
            q += 1;
        }
        let ef_legal = pages[p..q].iter().all(|pg| pg.ef_legal);
        free_holes.push(MemMapFreeHole { from_page: p, to_page: q - 1, pages: q - p, ef_legal });
        p = q;
    }

    let static_untouched: Vec<MemMapPage> = pages
        .iter()
        .filter(|pg| pg.static_occupied && pg.role == MemRole::Untouched)
        .cloned()
        .collect();

    let count = |f: &dyn Fn(&MemMapPage) -> bool| pages.iter().filter(|p| f(p)).count();
    let totals = MemMapTotals {
        code_pages: count(&|p| p.role == MemRole::Code || p.role == MemRole::CodeWrite),
        written_pages: count(&|p| {
            p.role == MemRole::DataW || p.role == MemRole::DataWMut || p.role == MemRole::CodeWrite
        }),
        read_pages: count(&|p| p.role == MemRole::DataR),
        untouched_pages: count(&|p| p.role == MemRole::Untouched),
        mutated_pages: count(&|p| p.mutations > 0),
        free_pages: count(&|p| p.provably_free),
    };

    MemMapResult {
        cpu: cpu.to_string(),
        pages,
        regions,
        free_holes,
        static_untouched,
        totals,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SQL — verbatim. `cpu` MUST be a validated literal ('c64' | 'drive8'); the
// monitor verb rejects anything else BEFORE calling ("map: cpu must be
// c64|drive8"), exactly like the TS reference, which interpolates raw too. The
// store handle is opened READ_ONLY first (see `conn::with_conn`) and
// `Connection::prepare` accepts a single statement, so this cannot mutate.
// ─────────────────────────────────────────────────────────────────────────────

/// Per-page write/read/mutation aggregate over `bus_events`.
pub fn mem_map_agg_sql(cpu: &str) -> String {
    format!(
        "SELECT (addr>>8) AS page, \
         COUNT(*) FILTER (WHERE kind='write') AS writes, \
         COUNT(*) FILTER (WHERE kind='read') AS reads, \
         COUNT(*) FILTER (WHERE kind='write' AND old_value IS NOT NULL AND old_value <> value) AS mutations, \
         MIN(clock) AS first_clk, MAX(clock) AS last_clk, \
         COUNT(DISTINCT pc) FILTER (WHERE kind='write') AS writer_pcs \
         FROM bus_events WHERE cpu='{cpu}' AND kind IN ('write','read') AND addr IS NOT NULL GROUP BY page ORDER BY page"
    )
}

/// The distinct pages that executed at least one instruction.
pub fn mem_map_code_sql(cpu: &str) -> String {
    format!(
        "SELECT DISTINCT (pc>>8) AS page FROM instructions WHERE cpu='{cpu}' AND pc IS NOT NULL"
    )
}

/// `N()` from the TS: `null`/missing → 0, everything else through `Number()`.
/// Every column here is an integer counter or a clock, so the float path is
/// only a guard.
fn num(v: Option<&serde_json::Value>) -> i64 {
    match v {
        None | Some(serde_json::Value::Null) => 0,
        Some(serde_json::Value::Bool(b)) => i64::from(*b),
        Some(serde_json::Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| u as i64))
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .unwrap_or(0),
        Some(serde_json::Value::String(s)) => {
            s.trim().parse::<f64>().map(|f| f.trunc() as i64).unwrap_or(0)
        }
        Some(_) => 0,
    }
}

/// Project the two query results onto the model.
pub fn build_memory_map_from_query_rows(
    agg_rows: &[Vec<serde_json::Value>],
    code_rows: &[Vec<serde_json::Value>],
    opts: &MapOptions,
) -> MemMapResult {
    let page_rows: Vec<MemMapPageRow> = agg_rows
        .iter()
        .map(|r| MemMapPageRow {
            page: num(r.first()),
            writes: num(r.get(1)),
            reads: num(r.get(2)),
            mutations: num(r.get(3)),
            first_clk: num(r.get(4)),
            last_clk: num(r.get(5)),
            writer_pcs: num(r.get(6)),
        })
        .collect();
    let code_pages: HashSet<usize> =
        code_rows.iter().map(|r| (num(r.first()) & 0xff) as usize).collect();
    build_memory_map(opts.cpu_or_default(), &page_rows, &code_pages, &opts.static_ranges)
}

/// Build the rendered map text from a store via a query runner.
///
/// Returns `None` when the trace captured no memory accesses (the aggregate
/// yields zero rows) — callers substitute [`MAP_EMPTY_TEXT`]. The code query is
/// **not** run in that case, matching the TS early return.
pub fn build_memory_map_text<F>(
    mut run_query: F,
    opts: &MapOptions,
) -> Result<Option<(String, MemMapResult)>>
where
    F: FnMut(&str) -> Result<Vec<Vec<serde_json::Value>>>,
{
    let cpu = opts.cpu_or_default();
    let agg_rows = run_query(&mem_map_agg_sql(cpu))?;
    if agg_rows.is_empty() {
        return Ok(None);
    }
    let code_rows = run_query(&mem_map_code_sql(cpu))?;
    let map = build_memory_map_from_query_rows(&agg_rows, &code_rows, opts);
    let text = render_memory_map(&map, opts.run_label.as_deref());
    Ok(Some((text, map)))
}

// ─────────────────────────────────────────────────────────────────────────────
// Layout
// ─────────────────────────────────────────────────────────────────────────────

fn page_char(pg: &MemMapPage) -> char {
    if pg.role == MemRole::Untouched {
        return if pg.static_occupied { '#' } else { '.' };
    }
    pg.role.role_char()
}

/// `hx(n, w)` — uppercase hex, zero-padded to `w`.
fn hx(n: usize, w: usize) -> String {
    format!("{n:0>w$X}", w = w)
}

fn page_addr(p: usize) -> String {
    format!("${}00", hx(p, 2))
}

/// `$XX00-$YYFF` for a page range.
fn page_range(from: usize, to: usize) -> String {
    format!("{}-${}FF", page_addr(from), hx(to, 2))
}

/// Render the model. `run_label` is only ever `Some` on the C64RE tool path.
pub fn render_memory_map(m: &MemMapResult, run_label: Option<&str>) -> String {
    let mut l: Vec<String> = Vec::new();
    l.push(format!(
        "# trace_memory_map — cpu={}{}",
        m.cpu,
        match run_label {
            Some(r) => format!("  run={r}"),
            None => String::new(),
        }
    ));
    l.push(String::new());
    l.push("⚠ COVERAGE = THIS RUN ONLY. A trace is ONE path. \"untouched\" ≠ \"free\":".into());
    l.push("  untested paths (other levels, battles, utils/save) may use a hole. Reconcile".into());
    l.push("  with the static module load-map / analysis-json before treating a hole as free.".into());
    l.push("  This is runtime BEHAVIOUR, NOT identity grounding.".into());
    l.push(String::new());
    l.push(format!(
        "totals: code={}p  written={}p  read-only={}p  untouched={}p  mutated={}p  provably-free={}p",
        m.totals.code_pages,
        m.totals.written_pages,
        m.totals.read_pages,
        m.totals.untouched_pages,
        m.totals.mutated_pages,
        m.totals.free_pages
    ));
    l.push(String::new());

    // ASCII page grid (rows = high nibble, cols = page low byte).
    l.push("page map (each cell = one $XX00 page; C=code c=code+write W=write M=write+mutated R=read-only .=free #=static-owned-untouched):".into());
    let head: Vec<String> = (0..16).map(|c| hx(c, 1)).collect();
    l.push(format!("      {}", head.join(" ")));
    for hi in 0..16usize {
        let cells: Vec<String> = (0..16usize)
            .map(|lo| page_char(&m.pages[(hi << 4) | lo]).to_string())
            .collect();
        l.push(format!("${}x00 {}", hx(hi, 1), cells.join(" ")));
    }
    l.push(String::new());

    // Region table (TAB separated, with a header row).
    l.push("regions:".into());
    l.push("range\trole\tpages\twrites\treads\tmut\twriterPCs\tstatic".into());
    for r in &m.regions {
        l.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            page_range(r.from_page, r.to_page),
            r.role.as_str(),
            r.to_page - r.from_page + 1,
            r.writes,
            r.reads,
            r.mutations,
            r.writer_pcs,
            r.static_label.as_deref().unwrap_or("-")
        ));
    }
    l.push(String::new());

    // Free holes.
    l.push(format!(
        "free holes (provably free = untouched this run AND not static-occupied) — {}:",
        m.free_holes.len()
    ));
    if m.free_holes.is_empty() {
        l.push("  (none)".into());
    }
    for h in &m.free_holes {
        l.push(format!(
            "  {}  {} page(s){}",
            page_range(h.from_page, h.to_page),
            h.pages,
            if h.ef_legal { "  [EF-legal RAM]" } else { "" }
        ));
    }

    if !m.static_untouched.is_empty() {
        l.push(String::new());
        l.push("static-owned but UNTOUCHED this run (NOT provably free — owner may use it on another path):".into());
        for pg in &m.static_untouched {
            l.push(format!(
                "  {}  owner={}",
                page_addr(pg.page),
                pg.static_label.as_deref().unwrap_or("?")
            ));
        }
    }

    l.join("\n")
}

// ─────────────────────────────────────────────────────────────────────────────
// Store entry points
// ─────────────────────────────────────────────────────────────────────────────

/// The monitor `map [c64|drive8]` verb.
///
/// `duckdb_path` is the INDEX path; its `.c64retrace` authority is the sibling
/// and is decoded lazily on first read (see [`crate::conn::with_conn`]).
pub fn render_map(duckdb_path: &Path, cpu: &str) -> Result<String> {
    render_map_with(duckdb_path, &MapOptions::for_cpu(cpu))
}

/// [`render_map`] plus the C64RE tool path's extras (`static_ranges`, `run_label`).
pub fn render_map_with(duckdb_path: &Path, opts: &MapOptions) -> Result<String> {
    let rendered = with_conn(duckdb_path, |conn, _shape| {
        build_memory_map_text(|sql| query_json(conn, sql), opts)
    })?;
    Ok(rendered.map(|(text, _)| text).unwrap_or_else(|| MAP_EMPTY_TEXT.to_string()))
}

/// The daemon/WS `map` op: `{"text": …}`.
///
/// Args are `{ "cpu": "c64" }`; a missing/`null` `cpu` defaults to `"c64"`,
/// matching the sidecar's `String(a.cpu ?? "c64")`.
pub fn op_map(duckdb_path: &Path, cpu: Option<&str>) -> Result<serde_json::Value> {
    let text = render_map(duckdb_path, cpu.unwrap_or("c64"))?;
    Ok(serde_json::json!({ "text": text }))
}

// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use duckdb::Connection;
    use serde_json::json;

    // ── pure model ──────────────────────────────────────────────────────────

    #[test]
    fn ef_legal_pages_are_0000_7fff_and_c000_cfff() {
        assert!(is_ef_legal_page(0x00));
        assert!(is_ef_legal_page(0x7f));
        assert!(!is_ef_legal_page(0x80));
        assert!(!is_ef_legal_page(0xbf));
        assert!(is_ef_legal_page(0xc0));
        assert!(is_ef_legal_page(0xcf));
        assert!(!is_ef_legal_page(0xd0));
        assert!(!is_ef_legal_page(0xff));
    }

    #[test]
    fn role_table_matches_the_ts_cascade() {
        assert_eq!(role_of(true, 5, 0, 0), MemRole::CodeWrite);
        assert_eq!(role_of(true, 0, 9, 3), MemRole::Code); // code wins over reads
        assert_eq!(role_of(false, 1, 0, 0), MemRole::DataW);
        assert_eq!(role_of(false, 1, 0, 1), MemRole::DataWMut);
        assert_eq!(role_of(false, 0, 1, 0), MemRole::DataR);
        assert_eq!(role_of(false, 0, 0, 0), MemRole::Untouched);
        // mutations without writes cannot happen, but must not promote.
        assert_eq!(role_of(false, 0, 0, 7), MemRole::Untouched);
    }

    #[test]
    fn page_chars_incl_static_hash() {
        let mk = |role: MemRole, st: bool| MemMapPage {
            page: 0,
            role,
            writes: 0,
            reads: 0,
            mutations: 0,
            writer_pcs: 0,
            first_clk: 0,
            last_clk: 0,
            static_occupied: st,
            static_label: None,
            provably_free: false,
            ef_legal: true,
        };
        assert_eq!(page_char(&mk(MemRole::Code, false)), 'C');
        assert_eq!(page_char(&mk(MemRole::CodeWrite, false)), 'c');
        assert_eq!(page_char(&mk(MemRole::DataW, false)), 'W');
        assert_eq!(page_char(&mk(MemRole::DataWMut, false)), 'M');
        assert_eq!(page_char(&mk(MemRole::DataR, false)), 'R');
        assert_eq!(page_char(&mk(MemRole::Untouched, false)), '.');
        assert_eq!(page_char(&mk(MemRole::Untouched, true)), '#');
        // a static-owned page that DID something keeps its role char
        assert_eq!(page_char(&mk(MemRole::DataW, true)), 'W');
    }

    #[test]
    fn hex_helpers() {
        assert_eq!(hx(0, 1), "0");
        assert_eq!(hx(10, 1), "A");
        assert_eq!(hx(0xcf, 2), "CF");
        assert_eq!(hx(1, 2), "01");
        assert_eq!(page_addr(0), "$0000");
        assert_eq!(page_addr(0xc0), "$C000");
        assert_eq!(page_range(0xc0, 0xcf), "$C000-$CFFF");
    }

    #[test]
    fn sql_is_verbatim() {
        assert_eq!(
            mem_map_agg_sql("c64"),
            "SELECT (addr>>8) AS page, COUNT(*) FILTER (WHERE kind='write') AS writes, \
             COUNT(*) FILTER (WHERE kind='read') AS reads, \
             COUNT(*) FILTER (WHERE kind='write' AND old_value IS NOT NULL AND old_value <> value) AS mutations, \
             MIN(clock) AS first_clk, MAX(clock) AS last_clk, \
             COUNT(DISTINCT pc) FILTER (WHERE kind='write') AS writer_pcs \
             FROM bus_events WHERE cpu='c64' AND kind IN ('write','read') AND addr IS NOT NULL GROUP BY page ORDER BY page"
        );
        assert_eq!(
            mem_map_code_sql("drive8"),
            "SELECT DISTINCT (pc>>8) AS page FROM instructions WHERE cpu='drive8' AND pc IS NOT NULL"
        );
    }

    #[test]
    fn num_matches_the_ts_n_helper() {
        assert_eq!(num(None), 0);
        assert_eq!(num(Some(&serde_json::Value::Null)), 0);
        assert_eq!(num(Some(&json!(17))), 17);
        assert_eq!(num(Some(&json!(4_294_967_296u64))), 4_294_967_296);
        assert_eq!(num(Some(&json!(3.9))), 3);
        assert_eq!(num(Some(&json!(true))), 1);
    }

    // ── layout: the byte-for-byte gate ──────────────────────────────────────

    /// A tiny map with one code page, one written page, one mutated page and
    /// one read-only page — asserted against the exact expected text.
    #[test]
    fn render_golden_text() {
        let agg = vec![
            // page, writes, reads, mutations, first_clk, last_clk, writer_pcs
            vec![json!(0x00), json!(17), json!(4), json!(0), json!(100), json!(900), json!(3)],
            vec![json!(0x04), json!(2), json!(0), json!(0), json!(120), json!(880), json!(1)],
            vec![json!(0xd0), json!(31), json!(9), json!(31), json!(7), json!(999), json!(5)],
            vec![json!(0xff), json!(0), json!(6), json!(0), json!(9), json!(99), json!(0)],
        ];
        let code = vec![vec![json!(0x08)], vec![json!(0x04)]];
        let map = build_memory_map_from_query_rows(&agg, &code, &MapOptions::for_cpu("c64"));

        // page 4 writes AND executes → code-write; page 8 executes only → code.
        assert_eq!(map.pages[0x00].role, MemRole::DataW);
        assert_eq!(map.pages[0x04].role, MemRole::CodeWrite);
        assert_eq!(map.pages[0x08].role, MemRole::Code);
        assert_eq!(map.pages[0xd0].role, MemRole::DataWMut);
        assert_eq!(map.pages[0xff].role, MemRole::DataR);
        assert_eq!(map.totals.code_pages, 2);
        assert_eq!(map.totals.written_pages, 3);
        assert_eq!(map.totals.read_pages, 1);
        assert_eq!(map.totals.untouched_pages, 251);
        assert_eq!(map.totals.mutated_pages, 1);
        assert_eq!(map.totals.free_pages, 251);

        let text = render_memory_map(&map, None);
        let expected = concat!(
            "# trace_memory_map — cpu=c64\n",
            "\n",
            "⚠ COVERAGE = THIS RUN ONLY. A trace is ONE path. \"untouched\" ≠ \"free\":\n",
            "  untested paths (other levels, battles, utils/save) may use a hole. Reconcile\n",
            "  with the static module load-map / analysis-json before treating a hole as free.\n",
            "  This is runtime BEHAVIOUR, NOT identity grounding.\n",
            "\n",
            "totals: code=2p  written=3p  read-only=1p  untouched=251p  mutated=1p  provably-free=251p\n",
            "\n",
            "page map (each cell = one $XX00 page; C=code c=code+write W=write M=write+mutated R=read-only .=free #=static-owned-untouched):\n",
            "      0 1 2 3 4 5 6 7 8 9 A B C D E F\n",
            "$0x00 W . . . c . . . C . . . . . . .\n",
            "$1x00 . . . . . . . . . . . . . . . .\n",
            "$2x00 . . . . . . . . . . . . . . . .\n",
            "$3x00 . . . . . . . . . . . . . . . .\n",
            "$4x00 . . . . . . . . . . . . . . . .\n",
            "$5x00 . . . . . . . . . . . . . . . .\n",
            "$6x00 . . . . . . . . . . . . . . . .\n",
            "$7x00 . . . . . . . . . . . . . . . .\n",
            "$8x00 . . . . . . . . . . . . . . . .\n",
            "$9x00 . . . . . . . . . . . . . . . .\n",
            "$Ax00 . . . . . . . . . . . . . . . .\n",
            "$Bx00 . . . . . . . . . . . . . . . .\n",
            "$Cx00 . . . . . . . . . . . . . . . .\n",
            "$Dx00 M . . . . . . . . . . . . . . .\n",
            "$Ex00 . . . . . . . . . . . . . . . .\n",
            "$Fx00 . . . . . . . . . . . . . . . R\n",
            "\n",
            "regions:\n",
            "range\trole\tpages\twrites\treads\tmut\twriterPCs\tstatic\n",
            "$0000-$00FF\tdata-w\t1\t17\t4\t0\t3\t-\n",
            "$0100-$03FF\tuntouched\t3\t0\t0\t0\t0\t-\n",
            "$0400-$04FF\tcode-write\t1\t2\t0\t0\t1\t-\n",
            "$0500-$07FF\tuntouched\t3\t0\t0\t0\t0\t-\n",
            "$0800-$08FF\tcode\t1\t0\t0\t0\t0\t-\n",
            "$0900-$CFFF\tuntouched\t199\t0\t0\t0\t0\t-\n",
            "$D000-$D0FF\tdata-w-mut\t1\t31\t9\t31\t5\t-\n",
            "$D100-$FEFF\tuntouched\t46\t0\t0\t0\t0\t-\n",
            "$FF00-$FFFF\tdata-r\t1\t0\t6\t0\t0\t-\n",
            "\n",
            "free holes (provably free = untouched this run AND not static-occupied) — 4:\n",
            "  $0100-$03FF  3 page(s)  [EF-legal RAM]\n",
            "  $0500-$07FF  3 page(s)  [EF-legal RAM]\n",
            "  $0900-$CFFF  199 page(s)\n",
            "  $D100-$FEFF  46 page(s)",
        );
        assert_eq!(text, expected);
    }

    /// `run_label`, `static_ranges`, the `#` grid cell, the `static` column and
    /// the trailing "static-owned but UNTOUCHED" block — the C64RE tool path.
    #[test]
    fn render_with_static_ranges_and_run_label() {
        let agg = vec![vec![json!(0x00), json!(1), json!(0), json!(0), json!(1), json!(2), json!(1)]];
        let opts = MapOptions {
            cpu: Some("c64".into()),
            static_ranges: vec![
                MemMapStaticRange { from: 0xa000, to: 0xa1ff, label: Some("loader".into()) },
            ],
            run_label: Some("run-7".into()),
        };
        let map = build_memory_map_from_query_rows(&agg, &[], &opts);
        assert!(map.pages[0xa0].static_occupied);
        assert!(!map.pages[0xa0].provably_free);
        assert_eq!(map.static_untouched.len(), 2);
        let text = render_memory_map(&map, opts.run_label.as_deref());

        assert!(text.starts_with("# trace_memory_map — cpu=c64  run=run-7\n"));
        assert!(text.contains("$Ax00 # # . . . . . . . . . . . . . .\n"));
        assert!(text.contains("$A000-$A1FF\tuntouched\t2\t0\t0\t0\t0\tloader\n"));
        // the untouched run is SPLIT on the static boundary
        assert!(text.contains("$0100-$9FFF\tuntouched\t159\t0\t0\t0\t0\t-\n"));
        assert!(text.contains("$A200-$FFFF\tuntouched\t94\t0\t0\t0\t0\t-\n"));
        // the static pages are excluded from the free holes
        assert!(text.contains("free holes (provably free = untouched this run AND not static-occupied) — 2:\n"));
        assert!(text.ends_with(concat!(
            "static-owned but UNTOUCHED this run (NOT provably free — owner may use it on another path):\n",
            "  $A000  owner=loader\n",
            "  $A100  owner=loader"
        )));
    }

    /// Every page touched ⇒ no free holes ⇒ the literal `  (none)` line.
    #[test]
    fn no_free_holes_renders_none() {
        let agg: Vec<Vec<serde_json::Value>> = (0..256)
            .map(|p| vec![json!(p), json!(1), json!(0), json!(0), json!(0), json!(0), json!(1)])
            .collect();
        let map = build_memory_map_from_query_rows(&agg, &[], &MapOptions::default());
        let text = render_memory_map(&map, None);
        assert!(text.contains(
            "free holes (provably free = untouched this run AND not static-occupied) — 0:\n  (none)"
        ));
        assert!(text.ends_with("  (none)"));
        // one region covering everything
        assert_eq!(map.regions.len(), 1);
        assert!(text.contains("$0000-$FFFF\tdata-w\t256\t256\t0\t0\t1\t-\n"));
    }

    #[test]
    fn cpu_defaults_to_c64() {
        let map = build_memory_map_from_query_rows(
            &[vec![json!(0), json!(0), json!(1), json!(0), json!(0), json!(0), json!(0)]],
            &[],
            &MapOptions::default(),
        );
        assert_eq!(map.cpu, "c64");
        assert!(render_memory_map(&map, None).starts_with("# trace_memory_map — cpu=c64\n"));
    }

    #[test]
    fn empty_aggregate_short_circuits_before_the_code_query() {
        let mut seen: Vec<String> = Vec::new();
        let out = build_memory_map_text(
            |sql| {
                seen.push(sql.to_string());
                Ok(Vec::new())
            },
            &MapOptions::for_cpu("c64"),
        )
        .unwrap();
        assert!(out.is_none());
        assert_eq!(seen.len(), 1, "the code query must not run: {seen:?}");
    }

    // ── against a real DuckDB store ─────────────────────────────────────────

    /// Build a Shape-B store in memory and populate `trace_event` so the compat
    /// views produce real rows; then run the actual SQL through it.
    fn seeded_store() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory duckdb");
        crate::schema::create_trace_run_store(&conn).expect("create store");
        // (channel, cycle, data_json)
        let rows: &[(&str, u64, String)] = &[
            // c64 writes to $C000 (page $C0): two writes, one of them a mutation.
            ("bus_access", 10, r#"{"addr":49152,"value":1,"op":"write","pc":4096,"side":"c64","oldValue":9,"cycle_c64":10}"#.into()),
            ("bus_access", 11, r#"{"addr":49153,"value":7,"op":"write","pc":4099,"side":"c64","oldValue":7,"cycle_c64":11}"#.into()),
            // a read on page $02
            ("bus_access", 12, r#"{"addr":512,"value":3,"op":"read","pc":4096,"side":"c64","cycle_c64":12}"#.into()),
            // an IO write on page $D0 with no oldValue → write, not a mutation
            ("io", 13, r#"{"addr":53280,"value":0,"op":"write","pc":4102,"side":"c64","cycle_c64":13}"#.into()),
            // a DRIVE write — must NOT show up for cpu='c64'
            ("bus_access", 14, r#"{"addr":1024,"value":5,"op":"write","pc":2048,"side":"drive","cycle_drive":14}"#.into()),
            // executed instructions: c64 at $1000 (page $10), drive at $0800
            ("cpu", 10, r#"{"pc":4096,"opcode":141,"b1":0,"b2":192,"a":1,"x":0,"y":0,"sp":253,"p":32}"#.into()),
            ("drive_pc", 14, r#"{"pc":2048,"opcode":141,"b1":0,"b2":4,"a":5,"x":0,"y":0,"sp":253,"p":32,"side":"drive","clk":14}"#.into()),
        ];
        for (seq, (channel, cycle, data)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO trace_event VALUES ('r1', ?, ?, ?, 'mem-access', 'mem-row', ?)",
                duckdb::params![seq as u64, cycle, channel, data],
            )
            .expect("insert trace_event");
        }
        conn
    }

    #[test]
    fn sql_runs_against_a_real_shape_b_store() {
        let conn = seeded_store();
        let (text, map) = build_memory_map_text(
            |sql| query_json(&conn, sql),
            &MapOptions::for_cpu("c64"),
        )
        .unwrap()
        .expect("agg rows");

        // $C0: 2 writes, 1 mutation (oldValue 9 != 1; oldValue 7 == 7 is not).
        assert_eq!(map.pages[0xc0].writes, 2);
        assert_eq!(map.pages[0xc0].mutations, 1);
        assert_eq!(map.pages[0xc0].writer_pcs, 2);
        assert_eq!(map.pages[0xc0].role, MemRole::DataWMut);
        // $02: read only.
        assert_eq!(map.pages[0x02].reads, 1);
        assert_eq!(map.pages[0x02].role, MemRole::DataR);
        // $D0: IO write, no oldValue ⇒ no mutation.
        assert_eq!(map.pages[0xd0].writes, 1);
        assert_eq!(map.pages[0xd0].mutations, 0);
        assert_eq!(map.pages[0xd0].role, MemRole::DataW);
        // $10: executed, never written.
        assert_eq!(map.pages[0x10].role, MemRole::Code);
        // the drive-side write/exec must not leak into the c64 map.
        assert_eq!(map.pages[0x04].role, MemRole::Untouched);
        assert_eq!(map.pages[0x08].role, MemRole::Untouched);

        assert!(text.contains("$C000-$C0FF\tdata-w-mut\t1\t2\t0\t1\t2\t-\n"));
        assert!(text.contains("$0200-$02FF\tdata-r\t1\t0\t1\t0\t0\t-\n"));
        assert!(text.contains("$1000-$10FF\tcode\t1\t0\t0\t0\t0\t-\n"));
    }

    #[test]
    fn drive8_sees_only_the_drive_side() {
        let conn = seeded_store();
        let (_, map) = build_memory_map_text(
            |sql| query_json(&conn, sql),
            &MapOptions::for_cpu("drive8"),
        )
        .unwrap()
        .expect("agg rows");
        assert_eq!(map.cpu, "drive8");
        assert_eq!(map.pages[0x04].role, MemRole::DataW); // $0400 write
        assert_eq!(map.pages[0x08].role, MemRole::Code); // $0800 executed
        assert_eq!(map.pages[0xc0].role, MemRole::Untouched); // c64-only
        assert_eq!(map.totals.code_pages, 1);
    }

    #[test]
    fn a_store_with_no_memory_rows_renders_the_empty_literal() {
        let dir = std::env::temp_dir().join(format!(
            "trx64-map-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("trace.duckdb");
        {
            let conn = Connection::open(&db).unwrap();
            crate::schema::create_trace_run_store(&conn).unwrap();
            // a CPU-only trace: instructions but no bus rows at all
            conn.execute(
                "INSERT INTO trace_event VALUES ('r1', 0, 10, 'cpu', 'pc-range', 'cpu-row', \
                 '{\"pc\":4096,\"opcode\":234,\"b1\":0,\"b2\":0,\"a\":0,\"x\":0,\"y\":0,\"sp\":253,\"p\":32}')",
                [],
            )
            .unwrap();
            conn.execute("CHECKPOINT", []).ok();
        }
        let text = render_map(&db, "c64").unwrap();
        assert_eq!(text, MAP_EMPTY_TEXT);
        let v = op_map(&db, None).unwrap();
        assert_eq!(v["text"].as_str().unwrap(), MAP_EMPTY_TEXT);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_map_end_to_end_over_a_file_store() {
        let dir = std::env::temp_dir().join(format!(
            "trx64-map-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("trace.duckdb");
        {
            let conn = Connection::open(&db).unwrap();
            crate::schema::create_trace_run_store(&conn).unwrap();
            conn.execute(
                "INSERT INTO trace_event VALUES ('r1', 0, 10, 'bus_access', 'mem-access', 'mem-row', \
                 '{\"addr\":49152,\"value\":1,\"op\":\"write\",\"pc\":4096,\"side\":\"c64\",\"oldValue\":9,\"cycle_c64\":10}')",
                [],
            )
            .unwrap();
            conn.execute("CHECKPOINT", []).ok();
        }
        let text = render_map(&db, "c64").unwrap();
        assert!(text.starts_with("# trace_memory_map — cpu=c64\n"));
        assert!(text.contains("$C000-$C0FF\tdata-w-mut\t1\t1\t0\t1\t1\t-\n"));
        assert!(text.contains("totals: code=0p  written=1p  read-only=0p  untouched=255p  mutated=1p  provably-free=255p\n"));
        assert_eq!(text.lines().filter(|l| l.starts_with('$') && l.contains("x00 ")).count(), 16);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Opt-in shape/channel dump for a real store — what the parity gate needs
    /// to know before it trusts a corpus entry (does it even HAVE bus rows?).
    /// `TRX64_TEST_DUCKDB=<trace.duckdb> cargo test -- --nocapture real_store_probe`
    #[test]
    fn real_store_probe() {
        let Ok(p) = std::env::var("TRX64_TEST_DUCKDB") else { return };
        let path = std::path::PathBuf::from(p);
        crate::conn::with_conn(&path, |conn, shape| {
            eprintln!("shape={shape:?}");
            for sql in [
                "SELECT table_name, table_type FROM information_schema.tables WHERE table_schema='main'",
                "SELECT cpu, kind, count(*) FROM bus_events GROUP BY cpu, kind",
                "SELECT cpu, count(*) FROM instructions GROUP BY cpu",
            ] {
                eprintln!("-- {sql}");
                match query_json(conn, sql) {
                    Ok(rows) => {
                        for r in rows.iter().take(20) {
                            eprintln!("   {r:?}");
                        }
                    }
                    Err(e) => eprintln!("   ERR {e}"),
                }
            }
            Ok(())
        })
        .unwrap();
    }

    /// Real captured trace, opt-in: `TRX64_TEST_DUCKDB=/path/to/trace.duckdb`
    /// (`TRX64_TEST_MAP_CPU` selects the cpu, default `c64`). A multi-GB store
    /// is far too slow for the default suite, so this is skipped unless the env
    /// var names a store. `--nocapture` prints the rendered map, which is what
    /// the sidecar diff consumes.
    #[test]
    fn real_store_smoke() {
        let Ok(p) = std::env::var("TRX64_TEST_DUCKDB") else {
            eprintln!("SKIP real_store_smoke — set TRX64_TEST_DUCKDB=<trace.duckdb>");
            return;
        };
        let cpu = std::env::var("TRX64_TEST_MAP_CPU").unwrap_or_else(|_| "c64".into());
        let path = std::path::PathBuf::from(p);
        let text = render_map(&path, &cpu).expect("render map");
        if text == MAP_EMPTY_TEXT {
            eprintln!("real_store_smoke: store has no memory rows (empty literal) — ok");
            return;
        }
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], format!("# trace_memory_map — cpu={cpu}"));
        assert_eq!(lines[1], "");
        assert_eq!(lines[6], ""); // banner block then blank
        assert!(lines[7].starts_with("totals: code="));
        assert_eq!(lines[8], "");
        assert!(lines[9].starts_with("page map (each cell = one $XX00 page;"));
        assert_eq!(lines[10], "      0 1 2 3 4 5 6 7 8 9 A B C D E F");
        for (i, hi) in (0..16usize).enumerate() {
            assert_eq!(lines[11 + i].len(), 37, "grid row width");
            assert!(lines[11 + i].starts_with(&format!("${}x00 ", hx(hi, 1))));
        }
        assert_eq!(lines[27], "");
        assert_eq!(lines[28], "regions:");
        assert_eq!(lines[29], "range\trole\tpages\twrites\treads\tmut\twriterPCs\tstatic");
        // regions must tile 0..255 exactly, in order
        let mut next = 0usize;
        for l in lines[30..].iter().take_while(|l| l.starts_with('$')) {
            let cols: Vec<&str> = l.split('\t').collect();
            assert_eq!(cols.len(), 8);
            let from = usize::from_str_radix(&cols[0][1..3], 16).unwrap();
            let to = usize::from_str_radix(&cols[0][7..9], 16).unwrap();
            assert_eq!(from, next);
            next = to + 1;
        }
        assert_eq!(next, 256, "regions must cover all 256 pages");
        eprintln!("{text}");
    }
}
