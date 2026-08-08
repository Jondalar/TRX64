//! `follow_path` — backwards causal-chain walk (Spec 802 F2 / C64RE Spec 233).
//!
//! Reference: `src/runtime/headless/v2/follow-path.ts` (`followPath(backend, q)`),
//! reached from the retired sidecar as `case "follow_path"`.
//!
//! # Built entirely on `query_events`
//!
//! The reference issues NO SQL of its own — every lookup
//! (`lastCpuStepBefore`, `lastMemWriteBefore`, `lastIrqAssertBefore`,
//! `lastIecChangeBefore`, `lastStackWriteBefore`, `findEndEvent`) is a
//! `queryEvents()` call. This port does the same: it calls
//! [`crate::query_events::query_events`] with the connection its op opened and
//! never writes a query of its own. That keeps the two ops' SQL identical by
//! construction — including the parts that are arguably wrong (see
//! [Reproduced quirks](#reproduced-quirks)).
//!
//! # Argument case — camelCase
//!
//! `{ runId, endEventCycle, endEventFamily, endEventKey, maxDepth,
//! cycleWindow, crossDomain }` (R3 §5). Every OTHER op except `taint` and
//! `query_events` uses snake_case.
//!
//! # Reproduced quirks
//!
//! This is a **port, not an improvement**. Every one of the following is
//! reference behaviour that a "sensible" implementation would get wrong, and
//! each is reproduced deliberately:
//!
//! * **Q1 — rule 1 starves rules 2/3/4/5.** `applyRules` tries
//!   `pc_predecessor` FIRST for `cpu_step` / `mem_write` / `mem_read` and
//!   *returns* on success. On any trace that carries the instruction domain,
//!   `mem_dep`, `stack_frame`, `irq_origin` and `io_dep` are therefore
//!   unreachable for those three families — the whole chain is a run of
//!   `pc_predecessor` hops, one instruction at a time. The later rules only
//!   fire when no `cpu_step` exists in `[cycleFloor, cycle-1]`.
//! * **Q2 — `lastIecChangeBefore` always answers `drive_atn_change`.** All
//!   three IEC families map to the SAME `bus_events.kind='line_change'` rows
//!   and differ only in which column becomes `level`, so the three queries
//!   return the same cycles. The tie-break is a strict `>`, so the first family
//!   tried (`drive_atn_change`) always wins.
//! * **Q3 — `LIMIT 10000` truncates from the FRONT.** The "last X before"
//!   helpers `ORDER BY clock` ascending, cap at 10000 and take
//!   `rows[rows.length - 1]`. In a window holding more than 10000 matches the
//!   answer is the 10000th oldest, NOT the newest. Ordering DESC would "fix"
//!   it and break parity.
//! * **Q4 — exhausting `maxDepth` does NOT set `truncated`.** Only hitting
//!   `cycleFloor` does.
//! * **Q5 — a non-number `endEventCycle` can never match.** `findEndEvent`
//!   compares `row.cycle === cycle` with JS strict equality against the RAW
//!   argument, so `"5000"` (a string) yields `{ steps: [], truncated: false }`
//!   even though the surrounding arithmetic coerces it fine. Modelled by
//!   [`PathQuery::end_event_cycle_is_number`].
//! * **Q6 — `stack_frame`'s address is NOT zero-padded.** Every other reason
//!   string pads to four hex digits; that one calls `.toUpperCase()` without
//!   `.padStart(4, "0")`, so it renders `$1FF`, not `$01FF`.
//! * **Q7 — `stack_frame` walks to *a* prior stack write, not to the JSR.**
//!   `lastStackWriteBefore` ignores the current address and returns the most
//!   recent write anywhere in `$0100..$01FF`; the rule's name is aspirational.
//! * **Q8 — an absent or `null` `endEventKey` makes the op THROW.**
//!   `rowMatchesKey` runs `Object.entries(key)`, which raises
//!   `TypeError: Cannot convert undefined or null to object`. It is a *latent*
//!   throw: `&&` short-circuits, so it only fires once a candidate row with the
//!   exact cycle exists — the same request against a cycle the store does not
//!   hold returns the empty chain instead. Measured against the sidecar (case
//!   `capsel1_cpu0_nokey`), reproduced by [`EndEventKey::Nullish`]. Reachable
//!   from the product path: `runtime_follow_path` does
//!   `JSON.parse(args.end_event_key)`, so the string `"null"` gets there.
//! * **Q9 — the op is NONDETERMINISTIC on a two-CPU trace.** `instructions`
//!   holds the C64 *and* the drive CPU, so two `cpu_step` rows can share a
//!   `clock`. `queryEvents` orders by `clock` with no tiebreaker and the
//!   `last*Before` helpers then take `rows[rows.length - 1]`, collapsing that
//!   tie — and DuckDB's parallel sort returns tied rows in an order that varies
//!   RUN TO RUN. Measured: the sidecar alone produced 3 distinct chains in 24
//!   runs of one query against one store (`scramble_atn`, end cycle 5362099);
//!   the native reader produced 4, a superset, with no sidecar-only variant.
//!   The variant space is the Cartesian product of the tie choices. Adding an
//!   `ORDER BY clock, seq` here would make the op stable but would change
//!   `query_events`' SQL, which is under its own parity contract — so the
//!   instability stands, and a parity gate must treat these cases as
//!   sampled-from-a-shared-set rather than equal.
//!
//!   Before "fixing" Q9: the tie is real data (two CPUs at one cycle), not a
//!   bug in the index. The stable fix belongs in the algorithm — pick the
//!   newest row explicitly instead of the last row of an unordered tie — and
//!   has to land on both sides at once.
//!
//! Known non-reproducible edge (documented, unreachable): if a `cpu_step` row
//! carried a NaN `pc`, JS would render `$0NAN` (optional chaining does not
//! short-circuit on NaN, and `"NaN".padStart(4,"0")` is `"0NaN"`). The native
//! projection turns a NaN into JSON `null`, which this port renders as the
//! `????` fallback. No store can produce it: `cpu_step.pc` is
//! `Number(r.pc)` over a non-nullable column.

use crate::conn::with_conn;
use crate::error::{Result, TraceReadError};
use crate::queries::js;
use crate::query_events::{self, EventFamily, EventQuery, EventRow};
use crate::schema::StoreShape;
use duckdb::Connection;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::path::Path;

/// V8's message for `Object.entries(undefined)` / `Object.entries(null)`.
///
/// The reference does not guard `endEventKey`, so this TypeError IS the op's
/// error text for a nullish key (quirk Q8).
const OBJECT_ENTRIES_TYPE_ERROR: &str = "Cannot convert undefined or null to object";

/// `endEventKey`, as `rowMatchesKey`'s `Object.entries(key)` sees it.
///
/// The reference types the field `Record<string, unknown>` and never validates
/// it, so every JSON type reaches `Object.entries` and behaves differently.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum EndEventKey {
    /// `undefined` (absent) or `null` → `Object.entries` throws (quirk Q8).
    #[default]
    Nullish,
    /// Everything else, enumerated exactly as `Object.entries` does it: an
    /// object's own keys; an array's indices (`"0"`, `"1"`, …); a string's
    /// characters; and NOTHING at all for a number or a boolean — which
    /// therefore matches every row, like `{}` does.
    Entries(Vec<(String, Value)>),
}

impl EndEventKey {
    fn from_arg(v: Option<&Value>) -> Self {
        match v {
            None | Some(Value::Null) => EndEventKey::Nullish,
            Some(Value::Object(m)) => {
                EndEventKey::Entries(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            }
            Some(Value::Array(a)) => EndEventKey::Entries(
                a.iter()
                    .enumerate()
                    .map(|(i, v)| (i.to_string(), v.clone()))
                    .collect(),
            ),
            // `Object.entries` splits a string by UTF-16 code unit; `chars()`
            // differs only for astral characters, and no row has a `"0"` key,
            // so any non-empty string mismatches either way.
            Some(Value::String(s)) => EndEventKey::Entries(
                s.chars()
                    .enumerate()
                    .map(|(i, c)| (i.to_string(), Value::String(c.to_string())))
                    .collect(),
            ),
            Some(Value::Bool(_)) | Some(Value::Number(_)) => EndEventKey::Entries(Vec::new()),
        }
    }
}

/// The `PathQuery` of follow-path.ts — **camelCase on the wire**.
#[derive(Clone, Debug, Default)]
pub struct PathQuery {
    pub run_id: Option<String>,
    /// `Number(args.endEventCycle)`; `NaN` when absent. Used for the ARITHMETIC
    /// (`cycleFloor`, the ±32 search window) — the MATCH goes through
    /// [`PathQuery::end_event_cycle_is_number`] as well (quirk Q5).
    pub end_event_cycle: f64,
    /// The raw `endEventCycle` argument, kept because the reference interpolates
    /// it verbatim into the end step's reason (`at cycle ${q.endEventCycle}`)
    /// and compares it with `===`. `None` = the key was absent.
    pub end_event_cycle_raw: Option<Value>,
    /// `None` = absent or unknown. `findEndEvent` then queries an unmapped
    /// family, `queryEvents` returns `[]`, and the chain is
    /// `{ steps: [], truncated: false }` — NOT an error.
    pub end_event_family: Option<EventFamily>,
    /// The family-specific predicate, matched field-by-field against the
    /// candidate row (`rowMatchesKey`). See [`EndEventKey`] — an absent or
    /// `null` argument makes the op throw (quirk Q8), it does NOT mean "match
    /// everything".
    pub end_event_key: EndEventKey,
    /// `q.maxDepth ?? 50` — apply with [`PathQuery::max_depth_or_default`].
    pub max_depth: Option<f64>,
    /// `q.cycleWindow ?? 100_000` — see [`PathQuery::cycle_window_or_default`].
    pub cycle_window: Option<f64>,
    /// `q.crossDomain !== false` — see [`PathQuery::cross_domain_or_default`].
    /// Stored raw so the "only a literal `false` disables it" rule is visible.
    pub cross_domain: Option<bool>,
}

impl PathQuery {
    /// Parse the WS `args` object. **camelCase** (R3 §5).
    pub fn from_camel(v: &Value) -> Self {
        PathQuery {
            run_id: js::opt_str(v, "runId"),
            end_event_cycle: js::number_arg(v.get("endEventCycle")),
            end_event_cycle_raw: v.get("endEventCycle").cloned(),
            end_event_family: match v.get("endEventFamily") {
                Some(Value::String(s)) => EventFamily::from_name(s),
                _ => None,
            },
            end_event_key: EndEventKey::from_arg(v.get("endEventKey")),
            max_depth: js::opt_num(v, "maxDepth"),
            cycle_window: js::opt_num(v, "cycleWindow"),
            cross_domain: v.get("crossDomain").and_then(Value::as_bool),
        }
    }

    /// `q.maxDepth ?? 50` — nullish coalescing: only absent / `null` defaults.
    pub fn max_depth_or_default(&self) -> f64 {
        self.max_depth.unwrap_or(50.0)
    }

    /// `q.cycleWindow ?? 100_000`.
    pub fn cycle_window_or_default(&self) -> f64 {
        self.cycle_window.unwrap_or(100_000.0)
    }

    /// `q.crossDomain !== false` — default TRUE, and only the literal `false`
    /// turns it off (a non-boolean argument leaves it on).
    pub fn cross_domain_or_default(&self) -> bool {
        self.cross_domain != Some(false)
    }

    /// `Math.max(0, q.endEventCycle - cycleWindow)`.
    pub fn cycle_floor(&self) -> f64 {
        (self.end_event_cycle - self.cycle_window_or_default()).max(0.0)
    }

    /// Quirk Q5: `findEndEvent` matches with `row.cycle === cycle` against the
    /// RAW argument. `row.cycle` is always a JS number, so a string / boolean /
    /// `null` / absent `endEventCycle` can never be `===` to it — the op then
    /// returns the empty chain regardless of what the store holds.
    pub fn end_event_cycle_is_number(&self) -> bool {
        matches!(self.end_event_cycle_raw, Some(Value::Number(_)))
    }

    /// `${q.endEventCycle}` — the raw argument in a template literal.
    fn end_event_cycle_text(&self) -> String {
        js::string_of(self.end_event_cycle_raw.as_ref())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Small JS-semantics helpers
// ─────────────────────────────────────────────────────────────────────────────

/// The three families `IEC_FAMILIES` holds.
const IEC_FAMILIES: [&str; 3] = ["drive_atn_change", "drive_clk_change", "drive_data_change"];

// Address constants as `f64`, because every comparison the reference makes is
// between JS numbers (Rust has no hex float literal).
const IO_LO: f64 = 0xd000 as f64;
const IO_HI: f64 = 0xddff as f64;
const STACK_LO: f64 = 0x0100 as f64;
const STACK_HI: f64 = 0x01ff as f64;
const CIA_LO: f64 = 0xdc00 as f64;
const CIA_HI: f64 = 0xddff as f64;
const IRQ_KERNAL_LO: f64 = 0xea31 as f64;
const IRQ_KERNAL_HI: f64 = 0xffff as f64;
const IRQ_ALT_LO: f64 = 0xfe43 as f64;
const IRQ_ALT_HI: f64 = 0xfebc as f64;

/// VIC `$D000-$D3FF`, SID `$D400-$D7FF`, CIA1 `$DC00-$DCFF`, CIA2 `$DD00-$DDFF`
/// — the reference collapses all of it into one `$D000..$DDFF` range.
fn is_io_address(addr: f64) -> bool {
    (IO_LO..=IO_HI).contains(&addr)
}

fn is_stack_address(addr: f64) -> bool {
    (STACK_LO..=STACK_HI).contains(&addr)
}

/// `row[key]` under JS *relational* coercion: an absent key is `undefined`
/// (`NaN`, so every comparison is false) and `null` coerces to `0`.
fn rel_num(row: &EventRow, key: &str) -> f64 {
    match row.get(key) {
        None => f64::NAN,
        Some(v) => js::to_f64(v).unwrap_or(f64::NAN),
    }
}

/// `${row.cycle}` — the value as a template literal renders it.
fn cycle_text(row: &EventRow) -> String {
    js::string_of(row.get("cycle"))
}

/// `row.family` (always set by `rowFromDb`; `""` if a caller hands us a row
/// without one).
fn family_of(row: &EventRow) -> &str {
    query_events::row_family(row)
}

/// JS `a === b`, where `a` is `row[k]` and an absent key means `undefined`.
///
/// Two distinct object/array literals are never `===` in JS, and `endEventKey`
/// comes from `JSON.parse`, so any structured key value can only ever mismatch.
fn js_strict_eq(a: Option<&Value>, b: &Value) -> bool {
    let Some(a) = a else { return false };
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            // Compare numerically — JSON `53248` and `53248.0` are the same JS
            // number even though serde models them differently.
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
        _ => false,
    }
}

/// `rowMatchesKey` — every entry of `key` must be `===` the row's field.
///
/// `Err` is the nullish-key TypeError (quirk Q8); the reference reaches it here
/// and nowhere else.
fn row_matches_key(row: &EventRow, key: &EndEventKey) -> Result<bool> {
    match key {
        EndEventKey::Nullish => Err(TraceReadError::other(OBJECT_ENTRIES_TYPE_ERROR)),
        EndEventKey::Entries(e) => Ok(e.iter().all(|(k, v)| js_strict_eq(row.get(k), v))),
    }
}

/// `n.toString(16).toUpperCase()`.
///
/// Fractions are truncated rather than rendered as hex fractions (JS would emit
/// `1F.8`); no store column can produce one — every `pc` / `addr` is an integer.
fn hex_upper(n: f64) -> String {
    if n.is_nan() {
        return "NAN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "INFINITY" } else { "-INFINITY" }.to_string();
    }
    let body = format!("{:X}", n.abs().trunc() as u128);
    if n < 0.0 {
        format!("-{body}")
    } else {
        body
    }
}

/// `.padStart(4, "0")`.
fn pad_start4(s: &str) -> String {
    let n = s.chars().count();
    if n >= 4 {
        s.to_string()
    } else {
        format!("{}{s}", "0".repeat(4 - n))
    }
}

/// `` `$${(row as any).pc?.toString(16).toUpperCase().padStart(4, "0") ?? "????"}` ``
///
/// Optional chaining short-circuits on `null`/`undefined` only — a `pc` of `0`
/// renders `$0000`.
fn pc_hex(row: &EventRow) -> String {
    match row.get("pc") {
        None | Some(Value::Null) => "$????".to_string(),
        Some(v) => format!("${}", pad_start4(&hex_upper(js::to_f64(v).unwrap_or(f64::NAN)))),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The walk
// ─────────────────────────────────────────────────────────────────────────────

struct RuleResult {
    rule: &'static str,
    event: EventRow,
    reason: String,
}

/// The reference's module-level functions, bound to one event source.
///
/// `fetch` is the single `queryEvents` door (see the module docs). It is a
/// closure rather than a `(&Connection, StoreShape)` pair purely so the walk
/// can be unit-tested against a synthetic store.
struct Walk<'a> {
    fetch: &'a mut dyn FnMut(&EventQuery) -> Result<Vec<EventRow>>,
    run_id: Option<String>,
}

impl Walk<'_> {
    fn query(
        &mut self,
        family: EventFamily,
        cycle_range: (f64, f64),
        addr_range: Option<(f64, f64)>,
        limit: f64,
    ) -> Result<Vec<EventRow>> {
        let q = EventQuery {
            run_id: self.run_id.clone(),
            family: Some(family),
            cycle_range: Some(cycle_range),
            pc_range: None,
            addr_range,
            predicate: None,
            limit: Some(limit),
        };
        (self.fetch)(&q)
    }

    /// `findEndEvent` — a ±32 cycle window, then exact-cycle + key match, then
    /// exact-cycle alone.
    fn find_end_event(&mut self, q: &PathQuery) -> Result<Option<EventRow>> {
        let Some(family) = q.end_event_family else {
            // `MAP[undefined]` misses → `queryEvents` returns [] → null.
            return Ok(None);
        };
        let cycle = q.end_event_cycle;
        const WINDOW: f64 = 32.0;
        let rows = self.query(family, (cycle - WINDOW, cycle + WINDOW), None, 200.0)?;
        // Quirk Q5: a non-number argument is never `===` a row's cycle.
        if !q.end_event_cycle_is_number() {
            return Ok(None);
        }
        for row in &rows {
            // `&&` short-circuits: `rowMatchesKey` (and with it quirk Q8's
            // throw) is only reached for a row at the EXACT cycle.
            if rel_num(row, "cycle") == cycle && row_matches_key(row, &q.end_event_key)? {
                return Ok(Some(row.clone()));
            }
        }
        // Relax: just match cycle.
        for row in &rows {
            if rel_num(row, "cycle") == cycle {
                return Ok(Some(row.clone()));
            }
        }
        Ok(None)
    }

    /// The shared shape of `last*Before`: ascending by clock, capped, take the
    /// last row (quirk Q3).
    fn last_before(
        &mut self,
        family: EventFamily,
        before_cycle: f64,
        cycle_floor: f64,
        addr_range: Option<(f64, f64)>,
    ) -> Result<Option<EventRow>> {
        let rows = self.query(
            family,
            (cycle_floor, before_cycle - 1.0),
            addr_range,
            10_000.0,
        )?;
        Ok(rows.into_iter().next_back())
    }

    fn last_cpu_step_before(&mut self, before: f64, floor: f64) -> Result<Option<EventRow>> {
        self.last_before(EventFamily::CpuStep, before, floor, None)
    }

    fn last_mem_write_before(
        &mut self,
        addr: f64,
        before: f64,
        floor: f64,
    ) -> Result<Option<EventRow>> {
        self.last_before(EventFamily::MemWrite, before, floor, Some((addr, addr)))
    }

    fn last_irq_assert_before(&mut self, before: f64, floor: f64) -> Result<Option<EventRow>> {
        self.last_before(EventFamily::IrqAssert, before, floor, None)
    }

    fn last_stack_write_before(&mut self, before: f64, floor: f64) -> Result<Option<EventRow>> {
        self.last_before(EventFamily::MemWrite, before, floor, Some((STACK_LO, STACK_HI)))
    }

    /// Try all three IEC line families, keep the latest. Quirk Q2: they read
    /// the same rows and the tie-break is strict `>`, so `drive_atn_change`
    /// always wins.
    fn last_iec_change_before(&mut self, before: f64, floor: f64) -> Result<Option<EventRow>> {
        let mut best: Option<EventRow> = None;
        for fam in [
            EventFamily::DriveAtnChange,
            EventFamily::DriveClkChange,
            EventFamily::DriveDataChange,
        ] {
            if let Some(last) = self.last_before(fam, before, floor, None)? {
                let better = match &best {
                    None => true,
                    Some(b) => rel_num(&last, "cycle") > rel_num(b, "cycle"),
                };
                if better {
                    best = Some(last);
                }
            }
        }
        Ok(best)
    }

    /// `applyRules` — the five rules IN THE REFERENCE'S ORDER (their order is
    /// their precedence), then the IEC cross-domain bridge, then the
    /// `pc_predecessor` fallback for every other family.
    fn apply_rules(
        &mut self,
        current: &EventRow,
        cycle_floor: f64,
        cross_domain: bool,
    ) -> Result<Option<RuleResult>> {
        let cycle = rel_num(current, "cycle");
        let family = family_of(current).to_string();
        let cycle_txt = cycle_text(current);

        // Rule 1: pc_predecessor — last cpu_step before this event.
        if family == "cpu_step" || family == "mem_write" || family == "mem_read" {
            if let Some(pred) = self.last_cpu_step_before(cycle, cycle_floor)? {
                let reason = format!(
                    "PC predecessor: last cpu_step at cycle {} (PC={}) before {family} at cycle {cycle_txt}",
                    cycle_text(&pred),
                    pc_hex(&pred),
                );
                return Ok(Some(RuleResult { rule: "pc_predecessor", event: pred, reason }));
            }
        }

        // Rule 5: io_dep — reading an IO register → find the last register write.
        if family == "mem_read" {
            let addr_defined = current.get("addr").is_some(); // `row.addr !== undefined`
            let addr = rel_num(current, "addr");
            if addr_defined && is_io_address(addr) {
                let addr_hex = format!("${}", pad_start4(&hex_upper(addr)));
                if let Some(writer) = self.last_mem_write_before(addr, cycle, cycle_floor)? {
                    let reason = format!(
                        "IO dependency: mem_read from {addr_hex} at cycle {cycle_txt} depends on mem_write at cycle {}",
                        cycle_text(&writer),
                    );
                    return Ok(Some(RuleResult { rule: "io_dep", event: writer, reason }));
                }
                // Try cross-domain: IEC line change.
                if cross_domain && (CIA_LO..=CIA_HI).contains(&addr) {
                    if let Some(iec) = self.last_iec_change_before(cycle, cycle_floor)? {
                        let reason = format!(
                            "IO cross-domain: CIA read at {addr_hex} cycle {cycle_txt} caused by IEC {} at cycle {}",
                            family_of(&iec),
                            cycle_text(&iec),
                        );
                        return Ok(Some(RuleResult { rule: "io_dep", event: iec, reason }));
                    }
                }
            }
        }

        // Rule 3: mem_dep — write to an address an earlier write also touched.
        if family == "mem_write" {
            let addr_defined = current.get("addr").is_some();
            let addr = rel_num(current, "addr");
            if addr_defined && !is_stack_address(addr) && !is_io_address(addr) {
                let addr_hex = format!("${}", pad_start4(&hex_upper(addr)));
                if let Some(prior) = self.last_mem_write_before(addr, cycle, cycle_floor)? {
                    let reason = format!(
                        "Memory dependency: write to {addr_hex} at cycle {cycle_txt} follows earlier write at cycle {}",
                        cycle_text(&prior),
                    );
                    return Ok(Some(RuleResult { rule: "mem_dep", event: prior, reason }));
                }
            }
        }

        // Rule 2: stack_frame — a stack touch walks to the prior stack write.
        if family == "mem_write" {
            let addr_defined = current.get("addr").is_some();
            let addr = rel_num(current, "addr");
            if addr_defined && is_stack_address(addr) {
                if let Some(jsr_write) = self.last_stack_write_before(cycle, cycle_floor)? {
                    // Quirk Q6: no `.padStart(4, "0")` on this one.
                    let reason = format!(
                        "Stack frame: stack write at ${} (cycle {cycle_txt}) walked to prior stack write at cycle {}",
                        hex_upper(addr),
                        cycle_text(&jsr_write),
                    );
                    return Ok(Some(RuleResult { rule: "stack_frame", event: jsr_write, reason }));
                }
            }
        }

        // Rule 4: irq_origin — a PC inside the KERNAL IRQ handler range.
        if family == "cpu_step" {
            // `row.pc ?? 0` — nullish, so an absent/null pc is 0.
            let pc = match current.get("pc") {
                None | Some(Value::Null) => 0.0,
                Some(v) => js::to_f64(v).unwrap_or(f64::NAN),
            };
            let in_irq_handler =
                (IRQ_KERNAL_LO..=IRQ_KERNAL_HI).contains(&pc) || (IRQ_ALT_LO..=IRQ_ALT_HI).contains(&pc);
            if in_irq_handler {
                if let Some(irq) = self.last_irq_assert_before(cycle, cycle_floor)? {
                    let reason = format!(
                        "IRQ origin: cpu_step at PC=${} (cycle {cycle_txt}) is in IRQ handler; irq_assert at cycle {}",
                        pad_start4(&hex_upper(pc)),
                        cycle_text(&irq),
                    );
                    return Ok(Some(RuleResult { rule: "irq_origin", event: irq, reason }));
                }
            }
        }

        // Cross-domain bridge (B2): an IEC boundary event hops to the opposite
        // domain's cycle region.
        if cross_domain && IEC_FAMILIES.contains(&family.as_str()) {
            if let Some(iec) = self.last_iec_change_before(cycle, cycle_floor)? {
                let reason = format!(
                    "IEC cross-domain bridge: {family} at cycle {cycle_txt} preceded by {} at cycle {}",
                    family_of(&iec),
                    cycle_text(&iec),
                );
                return Ok(Some(RuleResult { rule: "io_dep", event: iec, reason }));
            }
        }

        // Fallback: pc_predecessor for any remaining event.
        if family != "cpu_step" && family != "mem_write" && family != "mem_read" {
            if let Some(pred) = self.last_cpu_step_before(cycle, cycle_floor)? {
                let reason = format!(
                    "PC predecessor for {family}: last cpu_step at cycle {} (PC={})",
                    cycle_text(&pred),
                    pc_hex(&pred),
                );
                return Ok(Some(RuleResult { rule: "pc_predecessor", event: pred, reason }));
            }
        }

        Ok(None)
    }

    /// `followPath` proper.
    fn run(&mut self, q: &PathQuery) -> Result<Value> {
        let max_depth = q.max_depth_or_default();
        let cross_domain = q.cross_domain_or_default();
        let cycle_floor = q.cycle_floor();

        let Some(end_row) = self.find_end_event(q)? else {
            return Ok(chain(Vec::new(), false));
        };

        let mut steps_reversed: Vec<Value> = Vec::new();
        let mut current = end_row.clone();
        let mut truncated = false;

        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(visit_key(&current));

        // `for (let depth = 0; depth < maxDepth; depth++)` — an f64 counter so a
        // fractional or NaN `maxDepth` behaves exactly as it does in JS.
        let mut depth = 0.0f64;
        while depth < max_depth {
            if rel_num(&current, "cycle") <= cycle_floor {
                truncated = true;
                break;
            }

            let Some(result) = self.apply_rules(&current, cycle_floor, cross_domain)? else {
                break;
            };

            let key = visit_key(&result.event);
            if visited.contains(&key) {
                break;
            }
            visited.insert(key);

            steps_reversed.push(step(result.rule, &result.event, &result.reason));
            current = result.event;

            if rel_num(&current, "cycle") <= cycle_floor {
                truncated = true;
                break;
            }
            depth += 1.0;
        }

        // Earliest-first, then the end event itself with the sentinel rule.
        steps_reversed.reverse();
        let end_reason = format!(
            "End event: {} at cycle {}",
            q.end_event_family.map(EventFamily::as_str).unwrap_or(""),
            q.end_event_cycle_text(),
        );
        steps_reversed.push(step("pc_predecessor", &end_row, &end_reason));
        Ok(chain(steps_reversed, truncated))
    }
}

/// `` `${row.cycle}:${row.family}` `` — the `visited` set's key.
fn visit_key(row: &EventRow) -> String {
    format!("{}:{}", cycle_text(row), family_of(row))
}

fn step(rule: &str, event: &EventRow, reason: &str) -> Value {
    let mut o = Map::new();
    o.insert("rule".into(), Value::String(rule.to_string()));
    o.insert("event".into(), Value::Object(event.clone()));
    o.insert("reason".into(), Value::String(reason.to_string()));
    Value::Object(o)
}

fn chain(steps: Vec<Value>, truncated: bool) -> Value {
    let mut o = Map::new();
    o.insert("steps".into(), Value::Array(steps));
    o.insert("truncated".into(), Value::Bool(truncated));
    Value::Object(o)
}

/// Walk the causal chain backwards from an end event, against an OPEN store.
///
/// Returns the `PathChain` as JSON: `{ "steps": [ { rule, event, reason } … ],
/// "truncated": bool }`, steps EARLIEST-first with the end event appended last.
pub fn follow_path(conn: &Connection, shape: StoreShape, q: &PathQuery) -> Result<Value> {
    let mut fetch = |eq: &EventQuery| query_events::query_events(conn, shape, eq);
    Walk { fetch: &mut fetch, run_id: q.run_id.clone() }.run(q)
}

/// Op wrapper: `follow_path` (camelCase args) → the chain object.
pub fn op_follow_path(duckdb_path: &Path, args: &Value) -> Result<Value> {
    let q = PathQuery::from_camel(args);
    with_conn(duckdb_path, |conn, shape| follow_path(conn, shape, &q))
}

// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn args_are_camel_case() {
        let q = PathQuery::from_camel(&json!({
            "runId": "run-1",
            "endEventCycle": 5000,
            "endEventFamily": "mem_write",
            "endEventKey": { "addr": 53248 },
            "maxDepth": 12,
            "cycleWindow": 4000,
            "crossDomain": false
        }));
        assert_eq!(q.run_id.as_deref(), Some("run-1"));
        assert_eq!(q.end_event_cycle, 5000.0);
        assert_eq!(q.end_event_family, Some(EventFamily::MemWrite));
        assert_eq!(
            q.end_event_key,
            EndEventKey::Entries(vec![("addr".into(), json!(53248))])
        );
        assert_eq!(q.max_depth_or_default(), 12.0);
        assert_eq!(q.cycle_window_or_default(), 4000.0);
        assert!(!q.cross_domain_or_default());
        assert_eq!(q.cycle_floor(), 1000.0);

        // snake_case is a different op's convention and must not be read.
        let snake = PathQuery::from_camel(&json!({ "run_id": "r", "end_event_cycle": 5000 }));
        assert_eq!(snake.run_id, None);
        assert!(snake.end_event_cycle.is_nan());
    }

    #[test]
    fn defaults_match_the_reference() {
        let q = PathQuery::from_camel(&json!({ "endEventCycle": 100 }));
        assert_eq!(q.max_depth_or_default(), 50.0);
        assert_eq!(q.cycle_window_or_default(), 100_000.0);
        // `crossDomain !== false` — absent, null and a non-boolean all stay ON.
        assert!(q.cross_domain_or_default());
        assert!(PathQuery::from_camel(&json!({ "crossDomain": Value::Null }))
            .cross_domain_or_default());
        assert!(PathQuery::from_camel(&json!({ "crossDomain": 0 })).cross_domain_or_default());
        assert!(!PathQuery::from_camel(&json!({ "crossDomain": false })).cross_domain_or_default());
        // The floor never goes negative.
        assert_eq!(q.cycle_floor(), 0.0);
    }

    /// Quirk Q8 + every other `Object.entries` shape.
    #[test]
    fn end_event_key_shapes() {
        let k = |v: Value| PathQuery::from_camel(&v).end_event_key;
        assert_eq!(k(json!({})), EndEventKey::Nullish); // absent
        assert_eq!(k(json!({ "endEventKey": Value::Null })), EndEventKey::Nullish);
        assert_eq!(k(json!({ "endEventKey": {} })), EndEventKey::Entries(vec![]));
        // A number / boolean has no own enumerable properties → matches all.
        assert_eq!(k(json!({ "endEventKey": 7 })), EndEventKey::Entries(vec![]));
        assert_eq!(k(json!({ "endEventKey": true })), EndEventKey::Entries(vec![]));
        // An array enumerates as indices, a string as characters.
        assert_eq!(
            k(json!({ "endEventKey": [9, 8] })),
            EndEventKey::Entries(vec![("0".into(), json!(9)), ("1".into(), json!(8))])
        );
        assert_eq!(
            k(json!({ "endEventKey": "ab" })),
            EndEventKey::Entries(vec![("0".into(), json!("a")), ("1".into(), json!("b"))])
        );
    }

    // ── synthetic event source ───────────────────────────────────────────────
    //
    // A tiny in-memory `queryEvents` so the traversal can be pinned without a
    // store. It applies the same filters the real one does (family, run_id,
    // cycleRange, addrRange, ORDER BY clock, LIMIT) — that is all `follow_path`
    // depends on.

    fn row(family: &str, cycle: i64, fields: &[(&str, Value)]) -> EventRow {
        let mut m = Map::new();
        m.insert("runId".into(), json!("run-1"));
        m.insert("family".into(), json!(family));
        m.insert("cycle".into(), json!(cycle));
        for (k, v) in fields {
            m.insert((*k).into(), v.clone());
        }
        m
    }

    fn source(rows: Vec<EventRow>) -> impl FnMut(&EventQuery) -> Result<Vec<EventRow>> {
        move |q: &EventQuery| {
            let Some(fam) = q.family else { return Ok(vec![]) };
            let mut out: Vec<EventRow> = rows
                .iter()
                .filter(|r| family_of(r) == fam.as_str())
                .filter(|r| match q.cycle_range {
                    Some((lo, hi)) => {
                        let c = rel_num(r, "cycle");
                        c >= lo && c <= hi
                    }
                    None => true,
                })
                .filter(|r| match q.addr_range {
                    Some((lo, hi)) => {
                        let a = rel_num(r, "addr");
                        a >= lo && a <= hi
                    }
                    None => true,
                })
                .cloned()
                .collect();
            out.sort_by(|a, b| rel_num(a, "cycle").total_cmp(&rel_num(b, "cycle")));
            out.truncate(q.effective_limit() as usize);
            Ok(out)
        }
    }

    fn walk_res(rows: Vec<EventRow>, args: Value) -> Result<Value> {
        let q = PathQuery::from_camel(&args);
        let mut f = source(rows);
        Walk { fetch: &mut f, run_id: q.run_id.clone() }.run(&q)
    }

    fn walk(rows: Vec<EventRow>, args: Value) -> Value {
        walk_res(rows, args).unwrap()
    }

    fn reasons(v: &Value) -> Vec<String> {
        v["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["reason"].as_str().unwrap().to_string())
            .collect()
    }

    fn rules(v: &Value) -> Vec<String> {
        v["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["rule"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn walks_back_through_cpu_steps_earliest_first() {
        let rows = vec![
            row("cpu_step", 100, &[("pc", json!(0x0810))]),
            row("cpu_step", 102, &[("pc", json!(0x0813))]),
            row("cpu_step", 104, &[("pc", json!(0xea31))]),
            row("mem_write", 106, &[("pc", json!(0x0816)), ("addr", json!(0x2000)), ("value", json!(7))]),
        ];
        let out = walk(
            rows,
            json!({ "runId": "run-1", "endEventCycle": 106, "endEventFamily": "mem_write",
                    "endEventKey": {}, "maxDepth": 2, "cycleWindow": 1000 }),
        );
        // steps are earliest-first, end event last.
        assert_eq!(
            reasons(&out),
            vec![
                "PC predecessor: last cpu_step at cycle 102 (PC=$0813) before cpu_step at cycle 104",
                "PC predecessor: last cpu_step at cycle 104 (PC=$EA31) before mem_write at cycle 106",
                "End event: mem_write at cycle 106",
            ]
        );
        assert_eq!(out["truncated"], json!(false));
        // The end step carries the END event, not a predecessor.
        assert_eq!(out["steps"][2]["event"]["cycle"], json!(106));
        assert_eq!(out["steps"][0]["event"]["cycle"], json!(102));
    }

    #[test]
    fn q1_rule_one_starves_the_later_rules() {
        // A cpu_step inside the IRQ-handler range with BOTH an irq_assert and a
        // preceding cpu_step available: the reference returns pc_predecessor.
        let rows = vec![
            row("irq_assert", 50, &[("source", json!("cia1"))]),
            row("cpu_step", 90, &[("pc", json!(0x0810))]),
            row("cpu_step", 100, &[("pc", json!(0xea31))]),
        ];
        let out = walk(
            rows.clone(),
            json!({ "runId": "run-1", "endEventCycle": 100, "endEventFamily": "cpu_step",
                    "endEventKey": {}, "maxDepth": 1, "cycleWindow": 1000 }),
        );
        assert_eq!(rules(&out), vec!["pc_predecessor", "pc_predecessor"]);

        // Remove the earlier cpu_step and irq_origin becomes reachable.
        let thin = vec![rows[0].clone(), rows[2].clone()];
        let out = walk(
            thin,
            json!({ "runId": "run-1", "endEventCycle": 100, "endEventFamily": "cpu_step",
                    "endEventKey": {}, "maxDepth": 1, "cycleWindow": 1000 }),
        );
        assert_eq!(rules(&out), vec!["irq_origin", "pc_predecessor"]);
        assert_eq!(
            reasons(&out)[0],
            "IRQ origin: cpu_step at PC=$EA31 (cycle 100) is in IRQ handler; irq_assert at cycle 50"
        );
    }

    #[test]
    fn q6_stack_frame_address_is_not_padded() {
        let rows = vec![
            row("mem_write", 80, &[("pc", json!(0x0810)), ("addr", json!(0x01fe)), ("value", json!(0x08))]),
            row("mem_write", 90, &[("pc", json!(0x0812)), ("addr", json!(0x01ff)), ("value", json!(0x12))]),
        ];
        let out = walk(
            rows,
            json!({ "runId": "run-1", "endEventCycle": 90, "endEventFamily": "mem_write",
                    "endEventKey": {}, "maxDepth": 1, "cycleWindow": 1000 }),
        );
        assert_eq!(rules(&out), vec!["stack_frame", "pc_predecessor"]);
        assert_eq!(
            reasons(&out)[0],
            "Stack frame: stack write at $1FF (cycle 90) walked to prior stack write at cycle 80"
        );
    }

    #[test]
    fn io_dep_and_mem_dep_reasons() {
        // No cpu_step anywhere → rule 1 cannot fire, so rule 5 / rule 3 do.
        let io = vec![
            row("mem_write", 40, &[("pc", json!(0)), ("addr", json!(0xd020)), ("value", json!(0))]),
            row("mem_read", 60, &[("pc", json!(0)), ("addr", json!(0xd020)), ("value", json!(0))]),
        ];
        let out = walk(
            io,
            json!({ "runId": "run-1", "endEventCycle": 60, "endEventFamily": "mem_read",
                    "endEventKey": {}, "maxDepth": 1, "cycleWindow": 1000 }),
        );
        assert_eq!(rules(&out), vec!["io_dep", "pc_predecessor"]);
        assert_eq!(
            reasons(&out)[0],
            "IO dependency: mem_read from $D020 at cycle 60 depends on mem_write at cycle 40"
        );

        let mem = vec![
            row("mem_write", 40, &[("pc", json!(0)), ("addr", json!(0x2000)), ("value", json!(1))]),
            row("mem_write", 60, &[("pc", json!(0)), ("addr", json!(0x2000)), ("value", json!(2))]),
        ];
        let out = walk(
            mem,
            json!({ "runId": "run-1", "endEventCycle": 60, "endEventFamily": "mem_write",
                    "endEventKey": {}, "maxDepth": 1, "cycleWindow": 1000 }),
        );
        assert_eq!(rules(&out), vec!["mem_dep", "pc_predecessor"]);
        assert_eq!(
            reasons(&out)[0],
            "Memory dependency: write to $2000 at cycle 60 follows earlier write at cycle 40"
        );
    }

    #[test]
    fn q2_iec_bridge_always_answers_drive_atn_change() {
        // All three line families are the SAME rows in a real store; model that
        // by emitting one row per family at each line-change cycle.
        let mut rows = Vec::new();
        for c in [20i64, 30] {
            for fam in IEC_FAMILIES {
                rows.push(row(fam, c, &[("level", json!(1))]));
            }
        }
        let out = walk(
            rows,
            json!({ "runId": "run-1", "endEventCycle": 30, "endEventFamily": "drive_data_change",
                    "endEventKey": {}, "maxDepth": 1, "cycleWindow": 1000 }),
        );
        assert_eq!(rules(&out), vec!["io_dep", "pc_predecessor"]);
        assert_eq!(
            reasons(&out)[0],
            "IEC cross-domain bridge: drive_data_change at cycle 30 preceded by drive_atn_change at cycle 20"
        );
        assert_eq!(out["steps"][0]["event"]["family"], json!("drive_atn_change"));
    }

    #[test]
    fn cross_domain_false_falls_through_to_pc_predecessor() {
        let rows = vec![
            row("cpu_step", 10, &[("pc", json!(0xf0a4))]),
            row("drive_atn_change", 20, &[("level", json!(0))]),
            row("drive_atn_change", 30, &[("level", json!(1))]),
        ];
        let out = walk(
            rows,
            json!({ "runId": "run-1", "endEventCycle": 30, "endEventFamily": "drive_atn_change",
                    "endEventKey": {}, "maxDepth": 1, "cycleWindow": 1000, "crossDomain": false }),
        );
        assert_eq!(
            reasons(&out)[0],
            "PC predecessor for drive_atn_change: last cpu_step at cycle 10 (PC=$F0A4)"
        );
    }

    #[test]
    fn q4_exhausting_max_depth_does_not_set_truncated() {
        let rows: Vec<EventRow> = (0..10)
            .map(|i| row("cpu_step", 1000 + i * 2, &[("pc", json!(0x0810 + i))]))
            .collect();
        let out = walk(
            rows.clone(),
            json!({ "runId": "run-1", "endEventCycle": 1018, "endEventFamily": "cpu_step",
                    "endEventKey": {}, "maxDepth": 2, "cycleWindow": 100000 }),
        );
        assert_eq!(out["truncated"], json!(false));
        assert_eq!(out["steps"].as_array().unwrap().len(), 3);

        // Reaching the floor DOES set it.
        let out = walk(
            rows,
            json!({ "runId": "run-1", "endEventCycle": 1018, "endEventFamily": "cpu_step",
                    "endEventKey": {}, "maxDepth": 50, "cycleWindow": 4 }),
        );
        assert_eq!(out["truncated"], json!(true));
    }

    #[test]
    fn q5_string_end_event_cycle_never_matches() {
        let rows = vec![row("cpu_step", 100, &[("pc", json!(0x0810))])];
        let out = walk(
            rows.clone(),
            json!({ "runId": "run-1", "endEventCycle": "100", "endEventFamily": "cpu_step",
                    "endEventKey": {} }),
        );
        assert_eq!(out, json!({ "steps": [], "truncated": false }));

        // The same request with a real number finds it.
        let out = walk(
            rows,
            json!({ "runId": "run-1", "endEventCycle": 100, "endEventFamily": "cpu_step",
                    "endEventKey": {} }),
        );
        assert_eq!(out["steps"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn end_event_key_selects_among_same_cycle_rows() {
        // Two writes at the same cycle: the key picks one, and a key that
        // matches nothing relaxes to "first row at that cycle".
        let rows = vec![
            row("mem_write", 500, &[("pc", json!(0)), ("addr", json!(0x0400)), ("value", json!(1))]),
            row("mem_write", 500, &[("pc", json!(0)), ("addr", json!(0xd018)), ("value", json!(2))]),
        ];
        let pick = |key: Value| {
            let out = walk(
                rows.clone(),
                json!({ "runId": "run-1", "endEventCycle": 500, "endEventFamily": "mem_write",
                        "endEventKey": key, "maxDepth": 0 }),
            );
            out["steps"][0]["event"]["addr"].clone()
        };
        assert_eq!(pick(json!({ "addr": 53272 })), json!(53272));
        assert_eq!(pick(json!({ "addr": 1024 })), json!(1024));
        assert_eq!(pick(json!({})), json!(1024)); // first row wins
        assert_eq!(pick(json!({ "addr": 9999 })), json!(1024)); // relaxed
        // A string never equals a number under `===`.
        assert_eq!(pick(json!({ "addr": "53272" })), json!(1024));
        // Structured values are never `===` anything.
        assert_eq!(pick(json!({ "addr": [53272] })), json!(1024));
    }

    /// Quirk Q8 — measured against the sidecar on `capsel1`.
    #[test]
    fn q8_nullish_key_throws_only_at_an_exact_cycle_hit() {
        let rows = vec![row("cpu_step", 100, &[("pc", json!(0x0810))])];
        for key in [None, Some(Value::Null)] {
            let mut args = json!({ "runId": "run-1", "endEventCycle": 100, "endEventFamily": "cpu_step" });
            if let Some(k) = key {
                args["endEventKey"] = k;
            }
            let e = walk_res(rows.clone(), args).unwrap_err();
            assert_eq!(e.to_string(), "Cannot convert undefined or null to object");
        }
        // The SAME request one cycle off never calls `rowMatchesKey` (`&&`
        // short-circuits), so it is an empty chain rather than an error.
        let out = walk_res(
            rows,
            json!({ "runId": "run-1", "endEventCycle": 101, "endEventFamily": "cpu_step" }),
        )
        .unwrap();
        assert_eq!(out, json!({ "steps": [], "truncated": false }));
    }

    #[test]
    fn unknown_family_is_an_empty_chain_not_an_error() {
        let rows = vec![row("cpu_step", 100, &[("pc", json!(0x0810))])];
        for fam in [json!("not_a_family"), json!("vic_badline"), Value::Null] {
            let out = walk(
                rows.clone(),
                json!({ "runId": "run-1", "endEventCycle": 100, "endEventFamily": fam.clone(),
                        "endEventKey": {} }),
            );
            assert_eq!(out, json!({ "steps": [], "truncated": false }), "family {fam}");
        }
    }

    #[test]
    fn hex_helpers_match_the_js_expressions() {
        assert_eq!(pad_start4(&hex_upper(0.0)), "0000");
        assert_eq!(pad_start4(&hex_upper(0xea31 as f64)), "EA31");
        assert_eq!(pad_start4(&hex_upper(0x1ff as f64)), "01FF");
        assert_eq!(hex_upper(0x1ff as f64), "1FF"); // quirk Q6 renders this one
        assert_eq!(pad_start4(&hex_upper(0x12345 as f64)), "12345");
        let mut r = Map::new();
        r.insert("pc".into(), Value::Null);
        assert_eq!(pc_hex(&r), "$????");
        r.insert("pc".into(), json!(0));
        assert_eq!(pc_hex(&r), "$0000");
        assert_eq!(pc_hex(&Map::new()), "$????");
    }
}
