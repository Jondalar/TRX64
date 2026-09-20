//! Spec 864 §2 — the monitor's own state, and the session that owns it.
//!
//! Five things used to sit loose on the daemon's `State`: the cursors and the selected
//! device, the breakpoint surfaces, the observer registry with the DSL registrations
//! beside it, the interrupt-flow tracker, and the project's trap rules. None of them is
//! about being a daemon. They are what a monitor remembers between two commands, and a
//! second host needs every one of them.
//!
//! So they are gathered here into [`MonitorSession`]: one field on a host's own state,
//! and the thing a verb is handed along with the host itself.
//!
//! One deliberate omission, and it is the one §10.1 argues for: the filesystem cwd is
//! NOT here. It is shell state of a filesystem, and a host without a filesystem would be
//! carrying a directory it cannot use. It stays with the host, beside the files it
//! resolves against.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::observers::{ObsAction, ObsSpec, ObsTrigger, ObserverRegistry};

/// Simple numbered breakpoint (debug/break_* methods, numeric IDs).
pub struct BpEntry {
    pub num: u32,
    pub pc: u16,
    #[allow(dead_code)]
    pub enabled: bool,
}

/// String-ID breakpoint (api/call addPcBreakpoint/listBreakpoints/removeBreakpoint).
pub struct ApiBpEntry {
    pub id: String,
    pub pc: u16,
    pub action: String,
    pub enabled: bool,
    pub hit_limit: Option<u32>,
    /// `ignore <id> <n>` — skip the first N hits (VICE semantics, mirrored into
    /// the registry observer's `ignore_left`).
    pub ignore_count: u32,
    /// Real hit count, copied back from the registry after each run.
    pub hit_count: u64,
}

pub struct Breakpoints {
    pub next_num: u32,
    pub entries: Vec<BpEntry>,
    pub api_entries: Vec<ApiBpEntry>,
}

impl Default for Breakpoints {
    fn default() -> Self {
        Self::new()
    }
}

impl Breakpoints {
    pub fn new() -> Self {
        Self { next_num: 1, entries: Vec::new(), api_entries: Vec::new() }
    }

    pub fn list_vice_json(&self) -> Value {
        json!(self.entries.iter().map(|e| json!({
            "num": e.num as u64,
            "addr": e.pc as u64
        })).collect::<Vec<_>>())
    }
}

/// TRX64 feature-request #4 — one project-supplied on-trap dump rule. NO built-in
/// engine knowledge lives in the core: the PROJECT tells the debugger which bytes are
/// "the diagnosis" at a given trap PC. On reaching/halting at `pc`, the debugger reads
/// each `dump` byte and auto-emits `label: name=$XX name2=$YY (decode)`. Loaded from a
/// small JSON file via the `traprules <path>` verb. See `TRX64_FEATURE_REQUESTS.md` #4.
#[derive(Clone, Debug)]
pub struct TrapRule {
    /// The trap PC this rule fires at (reached / halted-at / breakpoint).
    pub pc: u16,
    /// Human label for the trap (e.g. "loader miss").
    pub label: String,
    /// The diagnostic bytes to read + name: `(name, addr, len)`. `len` is 1..=8 bytes,
    /// emitted as a single hex value (LE for len>1) under `name`.
    pub dump: Vec<(String, u16, u8)>,
    /// Optional human decode line appended in parentheses (e.g.
    /// "k2 bit7 => DIRECT-overlay miss"). Empty = omitted.
    pub decode: String,
}

/// T2.8 — the monitor-shell.ts module-level per-session state, collapsed for the
/// daemon's single session. `bank_default` = sticky lens for m/d (monitor-shell
/// `bankDefaults`, default "cpu"); `mem_cursor`/`disasm_cursor` = the shared
/// per-session cursors so a bare `m`/`d` follows the latest dump/step
/// (`memCursors`/`disasmCursors`); `sidefx_on` = side-effect read toggle
/// (`sidefxOn`, default OFF → peek).
///
/// The filesystem cwd is NOT here (Spec 864 §10.1): it is shell state of a filesystem,
/// and a host without one would be carrying a directory it cannot use. It lives with
/// the host, beside the paths it resolves.
pub struct MonitorState {
    pub bank_default: String,
    pub mem_cursor: Option<u16>,
    pub disasm_cursor: Option<u16>,
    pub sidefx_on: bool,
    /// Sticky inspect target (= monitor-shell `deviceSel`, default "c64"). When
    /// "drive8" the read-inspect verbs `r`/`m`/`d` target the 1541 drive CPU
    /// (read-inspect ONLY — Spec 754 §3.3i); other verbs are blocked with a clear
    /// message. `device c64|drive8` (or `dev`) flips it.
    pub device: String,
    /// Spec 754 §3.3c — modal assemble cursor (= monitor-shell `asmCursors`). When
    /// `Some(addr)` the monitor is in VICE-style `a` assemble mode: EVERY line is an
    /// instruction assembled at the cursor (no verb dispatch); an empty line exits.
    pub asm_cursor: Option<u16>,
    /// The `MonitorResult.prompt` for the LAST command (= the TS modal `prompt`
    /// field). Set per-command by `run_monitor` (cleared at entry); the `monitor/exec`
    /// handler forwards it on the reply so a modal `a`/`df -i` prompt reaches the wire
    /// exactly as TS's `runMonitorCommand` returns `{ output, prompt }`.
    pub pending_prompt: Option<String>,
}

impl Default for MonitorState {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorState {
    pub fn new() -> Self {
        Self {
            bank_default: "cpu".to_string(),
            mem_cursor: None,
            disasm_cursor: None,
            sidefx_on: false,
            device: "c64".to_string(),
            asm_cursor: None,
            pending_prompt: None,
        }
    }
}

/// Spec 623 §4.2/§4.3 — the per-session control-flow tracker, a 1:1 port of the
/// c64re TS `FlowTracker` (stepping.ts:145-281) that backs the monitor `flow`
/// panel (monitor-shell.ts:1103-1117 ← `ctrl.flow.flowState()`). It maintains the
/// interrupt/trap FRAME STACK so `flow` reports whether execution is currently in
/// main / irq / nmi flow — the LIVE interrupt context, not a constant.
///
/// STEP-DRIVEN, exactly like TS: the stack is mutated by [`FlowTracker::apply`],
/// which is called from the daemon's `z`/`step`/`n`/`ret` handlers after each
/// single-step (the TS `apply()` runs from `stepInto`/`stepOver`/`runReturn`/…).
/// A cold break from free-run leaves the stack empty → current=main (the documented
/// best-effort cold state, stepping.ts:142-143). The classification mirrors
/// `stepOne` (stepping.ts:78-103): an SP drop of exactly 3 across a step whose
/// pre-opcode is not BRK is the unambiguous hardware IRQ/NMI dispatch (no other
/// 6502 instruction pushes 3 bytes); BRK ($00) is a software interrupt entry; RTI
/// ($40) pops the innermost frame AFTER the RTI runs in handler flow.
///
/// PASSIVE OBSERVER (Spec 723 observer-effect lesson): the tracker reads CPU regs
/// the daemon already has post-step and reads the NMI vector via the non-side-effect
/// `peek_lens` — it never advances the VM, so it has ZERO effect on byte-exact
/// execution. The no-disk corpus is identical with it wired in.
///
/// FlowKind = main|irq|nmi|brk|trap (stepping.ts:39). BRK folds to its own `brk`
/// kind (TS classifies BRK entry as `brk`); `trap` is vestigial in the single-path
/// runtime. The 3-frame model (main/irq/nmi) plus `brk` matches stepping.ts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowKind {
    Main,
    Irq,
    Nmi,
    Brk,
}

impl FlowKind {
    /// The lowercase tag the TS render uses (`fr.kind` / `current=<kind>`).
    pub fn tag(self) -> &'static str {
        match self {
            FlowKind::Main => "main",
            FlowKind::Irq => "irq",
            FlowKind::Nmi => "nmi",
            FlowKind::Brk => "brk",
        }
    }
}

/// stepping.ts:44-51 — `CpuFlowFrame`. Field names mirror the TS interface; only the
/// fields the `flow` panel renders are carried (`stepping.ts:185-189`):
/// kind, enteredAtPc (→ `pc`), enteredAtCycle (→ `cyc`), returnPc (→ `ret`).
#[derive(Clone, Copy)]
pub struct CpuFlowFrame {
    pub kind: FlowKind,
    pub entered_at_pc: u16,
    pub entered_at_cycle: u64,
    pub return_pc: u16,
}

/// stepping.ts:78-103 — the classified result of one single step, used by
/// [`FlowTracker::apply`]. `ev` is the StepEventType; `flow` is set only for `int`.
#[derive(Debug, Clone)]
pub struct StepClass {
    pub is_int: bool,
    pub is_rti: bool,
    pub flow: FlowKind, // only meaningful when is_int
    pub pc0: u16,
    pub pc1: u16,
    pub cycle_abs: u64,
}

/// 1:1 port of the TS `FlowTracker` (stepping.ts:145-190). Only the state the
/// `flow` panel observes is carried; the stepping COMMANDS themselves stay in the
/// daemon's existing `z`/`n`/`ret` handlers (which already mirror stepInto/
/// stepOver/runReturn), and call [`FlowTracker::apply`] per single step.
pub struct FlowTracker {
    /// stepping.ts:146 — the interrupt/trap frame stack (innermost last).
    pub stack: Vec<CpuFlowFrame>,
    /// stepping.ts:147 — focus mode string (auto|main|irq|nmi|brk|none). The
    /// `flow` panel renders it verbatim; `focus` verb sets it. Default "auto".
    pub focus: String,
}

impl Default for FlowTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl FlowTracker {
    pub fn new() -> Self {
        FlowTracker { stack: Vec::new(), focus: "auto".to_string() }
    }

    /// stepping.ts:149-151 — currentFlow(): the innermost frame's kind, else main.
    pub fn current_flow(&self) -> FlowKind {
        self.stack.last().map(|f| f.kind).unwrap_or(FlowKind::Main)
    }

    /// stepping.ts:154-156 — the flow the focus verbs actually aim at. `auto`/`none`
    /// mean "whatever flow we are in right now"; anything else is that flow verbatim.
    pub fn effective_focus(&self) -> FlowKind {
        match self.focus.as_str() {
            "main" => FlowKind::Main,
            "irq" => FlowKind::Irq,
            "nmi" => FlowKind::Nmi,
            "brk" => FlowKind::Brk,
            _ => self.current_flow(),
        }
    }

    /// stepping.ts:158 — reset(): clear the frame stack (focus is left intact, as in
    /// TS where `reset()` only nulls `stack`).
    pub fn reset(&mut self) {
        self.stack.clear();
    }

    /// stepping.ts:160-171 — apply(): mutate the stack from a classified step. An
    /// `int` pushes a frame; an `rti` pops the innermost (AFTER the RTI ran in
    /// handler flow); jsr/rts/normal don't change the interrupt-flow kind.
    pub fn apply(&mut self, r: &StepClass) {
        if r.is_int {
            self.stack.push(CpuFlowFrame {
                kind: r.flow,
                entered_at_pc: r.pc1,
                entered_at_cycle: r.cycle_abs,
                return_pc: r.pc0,
            });
        } else if r.is_rti && !self.stack.is_empty() {
            self.stack.pop();
        }
    }

    /// monitor-shell.ts:1103-1117 — render the `flow` panel from flowState()
    /// (stepping.ts:174-190). Identical text shape:
    ///   `flow: current=<kind>  focus=<focus>\nframes:\n<lines | placeholder>`
    /// where each frame line is
    ///   `  <kind>  enter=$PPPP -> ret=$RRRR  cyc=<cycle>`.
    pub fn render(&self) -> String {
        let frames = if self.stack.is_empty() {
            "  (main — no interrupt/trap frame active)".to_string()
        } else {
            self.stack
                .iter()
                .map(|f| {
                    format!(
                        "  {}  enter=${:04X} -> ret=${:04X}  cyc={}",
                        f.kind.tag(),
                        f.entered_at_pc,
                        f.return_pc,
                        f.entered_at_cycle
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        format!(
            "flow: current={}  focus={}\nframes:\n{}",
            self.current_flow().tag(),
            self.focus,
            frames
        )
    }
}

/// Everything the monitor remembers between two commands.
///
/// The daemon carried these as five loose fields on a sixty-field `State`, which is why
/// it took a spec to notice they were a unit. They are: what the cursors are looking at,
/// what is armed, what the DSL registered, where the interrupt flow currently is, and
/// what the project says a trap means. A host owns ONE of these and hands it to the
/// monitor with itself.
///
/// The registry is live state, not a derived cache: `sync_observers` rebuilds it from
/// the breakpoint surfaces before every advance and preserves the hit and ignore counts
/// while doing so, which is the property that lets a host arm once and perform several
/// core runs before reporting back (§5.1).
pub struct MonitorSession {
    /// The cursors, the selected device, the assemble mode, the pending prompt.
    pub state: MonitorState,
    /// The breakpoint and watchpoint surfaces the verbs edit.
    pub breakpoints: Breakpoints,
    /// The breakpoint/watchpoint POLICY (cond-AST, hit/ignore, watch tables).
    /// Re-synced from `breakpoints` before each run; drives the core's debug gates.
    pub observers: ObserverRegistry,
    /// Spec 754 §3.3e — the persistent store of monitor-DSL observers registered via
    /// `obs <name> when exec|load|store $ADDR [if <cond>] do break|log|mark|cmd|trace`.
    /// A rebuild of the live registry would wipe them, so they are kept here and
    /// re-applied after the bp-derived ones.
    pub dsl_observers: Vec<ObsSpec>,
    /// Names of DSL observers currently DISABLED via `obs <name> off`. `ObsSpec` carries
    /// no enabled flag (the live `Observer` does, always re-armed enabled on `add`), so
    /// the disabled intent is persisted here and consulted on every rebuild.
    pub dsl_disabled: HashSet<String>,
    /// The per-session interrupt/trap flow-frame tracker behind `flow`/`focus`.
    pub flow: FlowTracker,
    /// Project-supplied on-trap dump rules, keyed by trap PC (last write wins on a
    /// duplicate PC). Empty by default — no built-in engine knowledge.
    pub trap_rules: HashMap<u16, TrapRule>,
}

impl MonitorSession {
    pub fn new() -> Self {
        Self {
            state: MonitorState::new(),
            breakpoints: Breakpoints::new(),
            observers: ObserverRegistry::new(),
            dsl_observers: Vec::new(),
            dsl_disabled: HashSet::new(),
            flow: FlowTracker::new(),
            trap_rules: HashMap::new(),
        }
    }
}

impl Default for MonitorSession {
    fn default() -> Self {
        Self::new()
    }
}

// ── the arming handshake (Spec 864 §3) ───────────────────────────────────────
//
// The registry is the run-time source of truth the core's debug gates consult; the
// breakpoint lists and the DSL store are the CRUD surfaces the verbs edit. A host
// re-syncs before an advance and writes the counts back after it, and because the sync
// preserves live hit and ignore counts it may re-arm between several core runs without
// losing them — the property §5.1 leans on.

/// Whether the current bp surface needs the breakpoint/observer driver at all.
pub fn observers_armed(reg: &ObserverRegistry) -> bool {
    reg.exec_active || reg.access_armed()
}

/// Re-sync the [`ObserverRegistry`] from the session's breakpoint surfaces
/// (`api_entries` string-ids + numbered `entries`) AND the persistent monitor-DSL
/// observer store, preserving each observer's accumulated `hits` / remaining
/// `ignore_left`. The registry is the run-time SOURCE OF TRUTH the core's debug
/// gates consult; the bp lists + the DSL store are the wire-shape CRUD stores.
/// After a run, [`writeback_hits`] copies the real hit counts back.
pub fn sync_observers(
    bp: &Breakpoints,
    dsl: &[ObsSpec],
    dsl_disabled: &HashSet<String>,
    reg: &mut ObserverRegistry,
) {
    // Snapshot current live counts so a rebuild doesn't reset them.
    let prior: HashMap<String, (u64, u64)> = reg
        .list()
        .iter()
        .map(|o| (o.name.clone(), (o.hits, o.ignore_left)))
        .collect();
    reg.clear();
    // String-id breakpoints (addPcBreakpoint / mem watchpoints).
    for e in &bp.api_entries {
        if !e.enabled {
            continue;
        }
        let (trigger, lo, hi, cond_src) = parse_api_bp(e);
        let action = if e.action == "log" {
            ObsAction::Log
        } else {
            ObsAction::Break
        };
        let _ = reg.add(ObsSpec {
            name: e.id.clone(),
            trigger,
            lo,
            hi,
            cond_src,
            action,
            log_exprs: None,
            cmd_src: None,
            mark_label: None,
            trace_scope: None,
        });
        // Restore live counts (default: fresh hits=0, ignore_left=ignore_count).
        let (hits, ignore_left) = prior
            .get(&e.id)
            .copied()
            .unwrap_or((e.hit_count, e.ignore_count as u64));
        reg.set_counts(&e.id, hits, ignore_left);
    }
    // Numbered exec breakpoints (debug/break_add).
    for e in &bp.entries {
        if !e.enabled {
            continue;
        }
        let name = format!("bp#{}", e.num);
        let _ = reg.add(ObsSpec {
            name: name.clone(),
            trigger: ObsTrigger::Exec,
            lo: e.pc,
            hi: e.pc,
            cond_src: None,
            action: ObsAction::Break,
            log_exprs: None,
            cmd_src: None,
            mark_label: None,
            trace_scope: None,
        });
        let (hits, ignore_left) = prior.get(&name).copied().unwrap_or((0, 0));
        reg.set_counts(&name, hits, ignore_left);
    }
    // Spec 754 §3.3e — persistent monitor-DSL observers (`obs … when … do …`). They
    // survive across runs (the c64re ensureObservers() registry), so re-apply a clone
    // of each onto the freshly-cleared registry, preserving live hit/ignore counts.
    // Registered AFTER the bp-derived ones; a same-name DSL observer replaces a
    // bp-derived one (add() replaces by name — DSL is the explicit, richer spec).
    for spec in dsl {
        let name = spec.name.clone();
        // A DSL observer turned `off` is not re-armed (the c64re Observer.enabled=false).
        if dsl_disabled.contains(&name) {
            continue;
        }
        let _ = reg.add(spec.clone());
        // Default for a DSL observer: keep accumulated counts; the `ignore` verb sets
        // ignore_left on the live registry, mirrored back below — but a fresh rebuild
        // restores from `prior` so the count is not lost mid-session.
        if let Some((hits, ignore_left)) = prior.get(&name).copied() {
            reg.set_counts(&name, hits, ignore_left);
        }
    }
}

/// Decode an [`ApiBpEntry`] into an observer trigger/range/cond. The `action`
/// field overloads as the watchpoint kind: "watch_read"/"watch_write"/"watch"
/// arm load/store observers; an `action` of the form "cond:<expr>" carries a
/// raw condition (the compact way to express a conditional bp over the
/// existing wire). Default = an exec breakpoint at the single PC.
pub fn parse_api_bp(e: &ApiBpEntry) -> (ObsTrigger, u16, u16, Option<String>) {
    if let Some(expr) = e.action.strip_prefix("cond:") {
        return (
            ObsTrigger::Exec,
            e.pc,
            e.pc,
            Some(expr.to_string()),
        );
    }
    match e.action.as_str() {
        "watch_read" | "load" => (ObsTrigger::Load, e.pc, e.pc, None),
        "watch_write" | "store" => (ObsTrigger::Store, e.pc, e.pc, None),
        "watch" => {
            // A read+write watch can't be one observer (single trigger); model it as
            // a store watch (the common debugging case). A separate load observer can
            // be added with action "watch_read" if needed.
            (ObsTrigger::Store, e.pc, e.pc, None)
        }
        _ => (ObsTrigger::Exec, e.pc, e.pc, None),
    }
}

/// Copy the real hit counts back from the registry into the session's bp surface
/// after a run, so `listBreakpoints` / `debug/break_list` report the true counts.
pub fn writeback_hits(bp: &mut Breakpoints, reg: &ObserverRegistry) {
    for e in bp.api_entries.iter_mut() {
        if let Some(o) = reg.get(&e.id) {
            e.hit_count = o.hits;
        }
    }
}

impl MonitorSession {
    /// Rebuild the live registry from this session's breakpoint surfaces and DSL store,
    /// preserving the accumulated counts. Call before an advance.
    pub fn sync_observers(&mut self) {
        sync_observers(
            &self.breakpoints,
            &self.dsl_observers,
            &self.dsl_disabled,
            &mut self.observers,
        );
    }

    /// Whether the current surface needs the breakpoint/observer driver at all.
    pub fn observers_armed(&self) -> bool {
        observers_armed(&self.observers)
    }

    /// Copy the real hit counts back after an advance.
    pub fn writeback_hits(&mut self) {
        writeback_hits(&mut self.breakpoints, &self.observers);
    }
}
