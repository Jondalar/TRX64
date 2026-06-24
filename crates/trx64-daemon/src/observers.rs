//! Breakpoint / watchpoint POLICY layer — a 1:1 port of the c64re
//! `ObserverRegistry` (src/runtime/headless/debug/monitor-observers.ts) over the
//! Phase-0 tick hooks landed in `trx64-core` (ADR-070).
//!
//! Spec 754 §3.3e — observers: the ONE abstraction that subsumes VICE's
//! break / watch / trace(point) / condition / command. An observer is
//!   { name, trigger, condition?, action }
//! and is evaluated IN the execution path (not run-then-rewind):
//!   - exec  triggers are checked at the instruction boundary (run loop),
//!   - load/store triggers fire from the CPU bus hook (read_raw/write_raw),
//! gated by a PER-ADDRESS watch table: idle cost is zero (the core's
//! `access_watch` is `None` when no load/store observer is active), and an active
//! observer only pays the condition eval on its EXACT address — no over-eval on
//! hot pages.
//!
//! v1 actions: `break` (halt at the trigger) and `log` (print + continue =
//! VICE tracepoint). `mark`/`cmd`/`trace <scope>` queue side-effects the
//! controller drains after the run chunk.
//!
//! POLICY lives HERE (daemon-side); `trx64-core` stays observer-agnostic. The
//! registry fills the `exec_watch[0x10000]` + `access_watch[0x10000]` presence
//! tables the core's debug run loop consumes, and implements the core
//! `trx64_core::Observer` trait so its `on_access` forwards the halt decision.

use std::collections::HashSet;

use trx64_core::{BusKind, Machine, Observer as CoreObserver};

// ts: monitor-observers.ts:18 — `export type ObsTrigger = "exec" | "load" | "store";`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObsTrigger {
    Exec,
    Load,
    Store,
}

// ts:23 — `export type ObsAction = "break" | "log" | "mark" | "cmd" | "trace";`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObsAction {
    Break,
    Log,
    Mark,
    Cmd,
    Trace,
}

/// ts:30 — `export type LogExpr` — a `do log` field: a register or a memory peek
/// (byte, or `:w` little-endian word).
#[derive(Clone, Debug)]
pub enum LogExpr {
    Reg(RegName),
    Mem { addr: u16, word: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegName {
    A,
    X,
    Y,
    Sp,
    Pc,
    Fl,
}

/// ts:40 — `export interface Observer`.
#[derive(Clone, Debug)]
pub struct Observer {
    pub name: String,
    pub trigger: ObsTrigger,
    pub lo: u16,
    pub hi: u16,
    pub cond_src: Option<String>,
    pub cond: Option<CondNode>,
    pub action: ObsAction,
    /// `do log <exprs>` fields; empty/absent = default line.
    pub log_exprs: Option<Vec<LogExpr>>,
    /// `do cmd "<mon-cmd>"` — command run on hit (v1.1).
    pub cmd_src: Option<String>,
    /// `do mark ["label"]` — trace bookmark on hit (v1.1).
    pub mark_label: Option<String>,
    /// `do trace [domains]|off` (v1.1).
    pub trace_scope: Option<TraceScope>,
    pub enabled: bool,
    pub hits: u64,
    pub ignore_left: u64,
}

#[derive(Clone, Debug)]
pub struct TraceScope {
    pub off: bool,
    pub domains: Vec<String>,
}

/// Spec for `add()` (ts:145 `add(spec: {...})`).
pub struct ObsSpec {
    pub name: String,
    pub trigger: ObsTrigger,
    pub lo: u16,
    pub hi: u16,
    pub cond_src: Option<String>,
    pub action: ObsAction,
    pub log_exprs: Option<Vec<LogExpr>>,
    pub cmd_src: Option<String>,
    pub mark_label: Option<String>,
    pub trace_scope: Option<TraceScope>,
}

// ---- condition AST + evaluator -----------------------------------------
// ts:58 — `type CondNode = { t:"num" } | { t:"id" } | { t:"bin" }`
#[derive(Clone, Debug, PartialEq)]
pub enum CondNode {
    Num(i64),
    Id(EnvId),
    Bin {
        op: CondOp,
        l: Box<CondNode>,
        r: Box<CondNode>,
    },
}

// ts:63 — `interface CondEnv { a,x,y,pc,sp,fl,rl,val,addr,cy }`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvId {
    A,
    X,
    Y,
    Pc,
    Sp,
    Fl,
    Rl,
    Val,
    Addr,
    Cy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CondOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
}

/// ts:63 — `interface CondEnv`.
#[derive(Clone, Copy, Debug, Default)]
pub struct CondEnv {
    pub a: i64,
    pub x: i64,
    pub y: i64,
    pub pc: i64,
    pub sp: i64,
    pub fl: i64,
    pub rl: i64,
    pub val: i64,
    pub addr: i64,
    pub cy: i64,
}

impl CondEnv {
    fn get(&self, id: EnvId) -> i64 {
        match id {
            EnvId::A => self.a,
            EnvId::X => self.x,
            EnvId::Y => self.y,
            EnvId::Pc => self.pc,
            EnvId::Sp => self.sp,
            EnvId::Fl => self.fl,
            EnvId::Rl => self.rl,
            EnvId::Val => self.val,
            EnvId::Addr => self.addr,
            EnvId::Cy => self.cy,
        }
    }
}

// ts:66 — `function evalNode(n, env): number`
fn eval_node(n: &CondNode, env: &CondEnv) -> i64 {
    match n {
        CondNode::Num(v) => *v,
        CondNode::Id(v) => env.get(*v),
        CondNode::Bin { op, l, r } => {
            let l = eval_node(l, env);
            let r = eval_node(r, env);
            match op {
                CondOp::Eq => (l == r) as i64,
                CondOp::Ne => (l != r) as i64,
                CondOp::Lt => (l < r) as i64,
                CondOp::Gt => (l > r) as i64,
                CondOp::Le => (l <= r) as i64,
                CondOp::Ge => (l >= r) as i64,
                // ts:77 `(l && r) ? 1 : 0` — JS truthiness: nonzero is truthy.
                CondOp::And => ((l != 0) && (r != 0)) as i64,
                CondOp::Or => ((l != 0) || (r != 0)) as i64,
            }
        }
    }
}

fn env_id_from_str(s: &str) -> Option<EnvId> {
    // ts:64 ID_NAMES = ["a","x","y","pc","sp","fl","rl","val","addr","cy"]
    Some(match s {
        "a" => EnvId::A,
        "x" => EnvId::X,
        "y" => EnvId::Y,
        "pc" => EnvId::Pc,
        "sp" => EnvId::Sp,
        "fl" => EnvId::Fl,
        "rl" => EnvId::Rl,
        "val" => EnvId::Val,
        "addr" => EnvId::Addr,
        "cy" => EnvId::Cy,
        _ => return None,
    })
}

/// ts:85 — the token regex `/<=|>=|==|!=|&&|\|\||[<>()]|\$[0-9a-fA-F]+|%[01]+|[0-9]+|[a-zA-Z]+/g`.
/// Tiny hand-rolled tokenizer that yields the same token stream.
fn tokenize(src: &str) -> Vec<String> {
    let b = src.as_bytes();
    let mut i = 0usize;
    let mut toks: Vec<String> = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // two-char operators: <= >= == != && ||
        if i + 1 < b.len() {
            let pair = &src[i..i + 2];
            if matches!(pair, "<=" | ">=" | "==" | "!=" | "&&" | "||") {
                toks.push(pair.to_string());
                i += 2;
                continue;
            }
        }
        // single-char: < > ( )
        if matches!(c, '<' | '>' | '(' | ')') {
            toks.push(c.to_string());
            i += 1;
            continue;
        }
        // $hex
        if c == '$' {
            let start = i;
            i += 1;
            while i < b.len() && (b[i] as char).is_ascii_hexdigit() {
                i += 1;
            }
            if i > start + 1 {
                toks.push(src[start..i].to_string());
                continue;
            }
            // lone `$` — the TS regex wouldn't match it; skip the char (no token).
            continue;
        }
        // %binary
        if c == '%' {
            let start = i;
            i += 1;
            while i < b.len() && matches!(b[i] as char, '0' | '1') {
                i += 1;
            }
            if i > start + 1 {
                toks.push(src[start..i].to_string());
                continue;
            }
            continue;
        }
        // decimal
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i] as char).is_ascii_digit() {
                i += 1;
            }
            toks.push(src[start..i].to_string());
            continue;
        }
        // identifier [a-zA-Z]+
        if c.is_ascii_alphabetic() {
            let start = i;
            while i < b.len() && (b[i] as char).is_ascii_alphabetic() {
                i += 1;
            }
            toks.push(src[start..i].to_string());
            continue;
        }
        // anything else: the TS regex would not match it — skip (drop the char).
        i += 1;
    }
    toks
}

/// ts:84 — `function parseCond(src): CondNode`. Recursive-descent over the
/// or → and → cmp → primary precedence ladder.
pub fn parse_cond(src: &str) -> Result<CondNode, String> {
    let toks = tokenize(src);
    let mut i = 0usize;

    fn op_of(t: &str) -> Option<CondOp> {
        Some(match t {
            "==" => CondOp::Eq,
            "!=" => CondOp::Ne,
            "<" => CondOp::Lt,
            ">" => CondOp::Gt,
            "<=" => CondOp::Le,
            ">=" => CondOp::Ge,
            _ => return None,
        })
    }

    // ts:89 parsePrimary
    fn parse_primary(toks: &[String], i: &mut usize) -> Result<CondNode, String> {
        if *i >= toks.len() {
            return Err("unexpected end of condition".to_string());
        }
        let t = toks[*i].clone();
        *i += 1;
        if t == "(" {
            let e = parse_or(toks, i)?;
            if *i >= toks.len() || toks[*i] != ")" {
                return Err("missing )".to_string());
            }
            *i += 1;
            return Ok(e);
        }
        if let Some(hex) = t.strip_prefix('$') {
            let v = i64::from_str_radix(hex, 16).map_err(|_| format!("bad hex '{t}'"))?;
            return Ok(CondNode::Num(v));
        }
        if let Some(bin) = t.strip_prefix('%') {
            let v = i64::from_str_radix(bin, 2).map_err(|_| format!("bad binary '{t}'"))?;
            return Ok(CondNode::Num(v));
        }
        if t.chars().all(|c| c.is_ascii_digit()) {
            let v: i64 = t.parse().map_err(|_| format!("bad number '{t}'"))?;
            return Ok(CondNode::Num(v));
        }
        let id = t.to_lowercase();
        if let Some(e) = env_id_from_str(&id) {
            return Ok(CondNode::Id(e));
        }
        Err(format!(
            "unknown term '{t}' (use a/x/y/pc/sp/fl/rl/val/addr/cy, $hex, == != < > <= >= && ||)"
        ))
    }

    // ts:100 parseCmp
    fn parse_cmp(toks: &[String], i: &mut usize) -> Result<CondNode, String> {
        let mut l = parse_primary(toks, i)?;
        while *i < toks.len() {
            if let Some(op) = op_of(&toks[*i]) {
                *i += 1;
                let r = parse_primary(toks, i)?;
                l = CondNode::Bin {
                    op,
                    l: Box::new(l),
                    r: Box::new(r),
                };
            } else {
                break;
            }
        }
        Ok(l)
    }

    // ts:107 parseAnd
    fn parse_and(toks: &[String], i: &mut usize) -> Result<CondNode, String> {
        let mut l = parse_cmp(toks, i)?;
        while *i < toks.len() && toks[*i] == "&&" {
            *i += 1;
            let r = parse_cmp(toks, i)?;
            l = CondNode::Bin {
                op: CondOp::And,
                l: Box::new(l),
                r: Box::new(r),
            };
        }
        Ok(l)
    }

    // ts:112 parseOr
    fn parse_or(toks: &[String], i: &mut usize) -> Result<CondNode, String> {
        let mut l = parse_and(toks, i)?;
        while *i < toks.len() && toks[*i] == "||" {
            *i += 1;
            let r = parse_and(toks, i)?;
            l = CondNode::Bin {
                op: CondOp::Or,
                l: Box::new(l),
                r: Box::new(r),
            };
        }
        Ok(l)
    }

    let tree = parse_or(&toks, &mut i)?;
    if i < toks.len() {
        return Err(format!("trailing '{}' in condition", toks[i]));
    }
    Ok(tree)
}

/// The snapshot of CPU + raster state the registry needs to evaluate a condition
/// / render a log line. Gathered from the `Machine` (live verbatim core) right
/// before a trigger fires, so eval/log run against the at-trigger state.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuSnapshot {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub pc: u16,
    pub sp: u8,
    pub fl: u8,
    /// Raster line: `(peek($d012) | ((peek($d011) & 0x80) << 1)) & 0x1ff`.
    pub rl: u16,
    pub cy: u64,
}

impl CpuSnapshot {
    /// ts:236 — build the env from the live CPU + the $d011/$d012 raster peek.
    pub fn from_machine(m: &Machine) -> Self {
        let c = &m.c64_core;
        let d012 = m.read_full(0xd012) as u16;
        let d011 = m.read_full(0xd011) as u16;
        let rl = (d012 | ((d011 & 0x80) << 1)) & 0x1ff;
        Self {
            a: c.reg_a,
            x: c.reg_x,
            y: c.reg_y,
            pc: c.reg_pc,
            sp: c.reg_sp,
            fl: c.status(),
            rl,
            cy: c.clk,
        }
    }
}

/// What halted the run (the registry's `lastHalt`, ts:128).
#[derive(Clone, Debug)]
pub struct HaltInfo {
    pub name: String,
    pub message: String,
    pub pc: u16,
}

// ---- the registry -------------------------------------------------------
// ts:123 — `export class ObserverRegistry`.
pub struct ObserverRegistry {
    /// ts:124 — per-PC exec-watch presence table.
    pub exec_watch: Box<[u8; 0x10000]>,
    /// ts:125 — per-address load/store-watch presence table.
    pub access_watch: Box<[u8; 0x10000]>,
    /// ts:126 — at least one enabled exec observer.
    pub exec_active: bool,
    /// At least one enabled load/store observer (= cpu.accessWatch non-null, ts:196).
    pub access_active: bool,
    /// ts:127 — `haltRequested` (set by an access break, honored at the boundary).
    pub halt_requested: bool,
    /// ts:128 — `lastHalt`.
    pub last_halt: Option<HaltInfo>,
    /// ts:129 — ring of recent `do log` lines.
    pub logs: Vec<String>,
    pending_log: Vec<String>,
    pending_marks: Vec<String>,
    pending_cmds: Vec<String>,
    pending_trace: Vec<(bool, Vec<String>, String)>,
    observers: Vec<Observer>,
    /// The at-trigger CPU snapshot the daemon refreshes before each segment +
    /// at each `on_access` hit (Rust can't reach back into the Machine from the
    /// trait method, so the daemon pushes it in via [`Self::set_env`]).
    env: CpuSnapshot,
}

impl Default for ObserverRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ObserverRegistry {
    pub fn new() -> Self {
        Self {
            exec_watch: Box::new([0u8; 0x10000]),
            access_watch: Box::new([0u8; 0x10000]),
            exec_active: false,
            access_active: false,
            halt_requested: false,
            last_halt: None,
            logs: Vec::new(),
            pending_log: Vec::new(),
            pending_marks: Vec::new(),
            pending_cmds: Vec::new(),
            pending_trace: Vec::new(),
            observers: Vec::new(),
            env: CpuSnapshot::default(),
        }
    }

    /// ts:145 — `add(spec): Observer | { error }`. Parse + register; an existing
    /// observer with the same name is REPLACED. Rebuilds the watch tables.
    pub fn add(&mut self, spec: ObsSpec) -> Result<Observer, String> {
        let cond = match &spec.cond_src {
            Some(src) => Some(parse_cond(src)?),
            None => None,
        };
        let log_exprs = match spec.log_exprs {
            Some(v) if !v.is_empty() => Some(v),
            _ => None,
        };
        let obs = Observer {
            name: spec.name.clone(),
            trigger: spec.trigger,
            lo: spec.lo,
            hi: spec.hi,
            cond_src: spec.cond_src,
            cond,
            action: spec.action,
            log_exprs,
            cmd_src: spec.cmd_src,
            mark_label: spec.mark_label,
            trace_scope: spec.trace_scope,
            enabled: true,
            hits: 0,
            ignore_left: 0,
        };
        if let Some(slot) = self.observers.iter_mut().find(|o| o.name == spec.name) {
            *slot = obs.clone();
        } else {
            self.observers.push(obs.clone());
        }
        self.rebuild();
        Ok(obs)
    }

    /// ts:163 — `remove(name): boolean`.
    pub fn remove(&mut self, name: &str) -> bool {
        let n = self.observers.len();
        self.observers.retain(|o| o.name != name);
        if self.observers.len() != n {
            self.rebuild();
            true
        } else {
            false
        }
    }

    /// ts:169 — `setEnabled(name, on): boolean`.
    pub fn set_enabled(&mut self, name: &str, on: bool) -> bool {
        if let Some(o) = self.observers.iter_mut().find(|x| x.name == name) {
            o.enabled = on;
            self.rebuild();
            true
        } else {
            false
        }
    }

    /// ts:174 — `setIgnore(name, n): boolean`.
    pub fn set_ignore(&mut self, name: &str, n: i64) -> bool {
        if let Some(o) = self.observers.iter_mut().find(|x| x.name == name) {
            o.ignore_left = n.max(0) as u64;
            true
        } else {
            false
        }
    }

    /// ts:179 — `list()`.
    pub fn list(&self) -> &[Observer] {
        &self.observers
    }

    /// Look up a single observer by name (for hitCount reporting).
    pub fn get(&self, name: &str) -> Option<&Observer> {
        self.observers.iter().find(|o| o.name == name)
    }

    /// ts:180 — `get active(): boolean` — any enabled observer.
    pub fn active(&self) -> bool {
        self.observers.iter().any(|o| o.enabled)
    }

    /// ts:183 — `rebuild()`. Recompute the per-address watch tables from the
    /// enabled set. The daemon arms the core's gates from `exec_watch` (as the
    /// breakpoint HashSet) + `access_watch` (as the watch table).
    pub fn rebuild(&mut self) {
        self.exec_watch.fill(0);
        self.access_watch.fill(0);
        self.exec_active = false;
        let mut any_access = false;
        for o in &self.observers {
            if !o.enabled {
                continue;
            }
            let exec = o.trigger == ObsTrigger::Exec;
            let lo = o.lo as usize;
            let hi = o.hi as usize;
            let tbl = if exec {
                &mut self.exec_watch
            } else {
                &mut self.access_watch
            };
            for a in lo..=hi {
                tbl[a & 0xffff] = 1;
            }
            if exec {
                self.exec_active = true;
            } else {
                any_access = true;
            }
        }
        self.access_active = any_access;
    }

    /// The set of exec-breakpoint PCs to arm in the core's `breakpoints` HashSet
    /// (the core halts AT the PC; the daemon then consults `on_exec` for the
    /// cond/ignore gate). Returns `None` when no exec observer is armed (zero-cost).
    pub fn exec_breakpoint_set(&self) -> Option<HashSet<u16>> {
        if !self.exec_active {
            return None;
        }
        let mut set = HashSet::new();
        for o in &self.observers {
            if !o.enabled || o.trigger != ObsTrigger::Exec {
                continue;
            }
            for a in o.lo..=o.hi {
                set.insert(a);
            }
        }
        Some(set)
    }

    /// Push the at-trigger CPU snapshot in from the daemon before evaluating
    /// exec/access conditions (the trait `on_access` needs it, and `on_exec` is
    /// called by the daemon with the env already set).
    pub fn set_env(&mut self, env: CpuSnapshot) {
        self.env = env;
    }

    /// ts:206 — `onExec(pc): boolean`. Called by the run-segment loop at the
    /// breakpoint boundary BEFORE executing the instruction at `pc`. Returns
    /// true if a break-action matched → the caller halts with PC AT `pc`. The
    /// env must be set (via [`set_env`]) to the at-`pc` CPU state first.
    pub fn on_exec(&mut self, pc: u16) -> bool {
        let mut halt = false;
        // Index by position to satisfy the borrow checker (matches/fire borrow &self).
        for idx in 0..self.observers.len() {
            let o = &self.observers[idx];
            if !o.enabled || o.trigger != ObsTrigger::Exec || pc < o.lo || pc > o.hi {
                continue;
            }
            if !self.matches(idx, pc, pc, 0) {
                continue;
            }
            if self.fire(idx, pc, None, None) {
                halt = true;
                self.last_halt = Some(HaltInfo {
                    name: self.observers[idx].name.clone(),
                    message: format!("exec ${:04X}", pc),
                    pc,
                });
            }
        }
        halt
    }

    /// ts:217 — `onAccess(kind, addr, value)`. Called by the core bus hook
    /// (via the [`CoreObserver`] impl) during an instruction. Sets
    /// `halt_requested` if a break matched; honored at the NEXT boundary.
    pub fn on_access_policy(&mut self, kind: BusKind, addr: u16, value: u8) {
        let want = if kind == BusKind::Write {
            ObsTrigger::Store
        } else {
            ObsTrigger::Load
        };
        let pc = self.env.pc;
        for idx in 0..self.observers.len() {
            let o = &self.observers[idx];
            if !o.enabled || o.trigger != want || addr < o.lo || addr > o.hi {
                continue;
            }
            if !self.matches(idx, pc, addr, value) {
                continue;
            }
            if self.fire(idx, pc, Some(value), Some(addr)) {
                self.halt_requested = true;
                let want_str = if want == ObsTrigger::Store { "store" } else { "load" };
                self.last_halt = Some(HaltInfo {
                    name: self.observers[idx].name.clone(),
                    message: format!("{want_str} ${:04X}=${:02X}", addr, value),
                    pc,
                });
            }
        }
    }

    /// ts:231 — `matches(o, pc, addr, value): boolean`. cond + ignore-count gate;
    /// bumps `hits` when it actually triggers.
    fn matches(&mut self, idx: usize, pc: u16, addr: u16, value: u8) -> bool {
        if let Some(cond) = self.observers[idx].cond.clone() {
            let e = self.env;
            let env = CondEnv {
                a: e.a as i64,
                x: e.x as i64,
                y: e.y as i64,
                pc: (pc & 0xffff) as i64,
                sp: e.sp as i64,
                fl: e.fl as i64,
                rl: e.rl as i64,
                val: (value & 0xff) as i64,
                addr: (addr & 0xffff) as i64,
                cy: e.cy as i64,
            };
            if eval_node(&cond, &env) == 0 {
                return false;
            }
        }
        let o = &mut self.observers[idx];
        if o.ignore_left > 0 {
            o.ignore_left -= 1;
            return false;
        }
        o.hits += 1;
        true
    }

    /// ts:245 — `fire(o, pc, value?, addr?): boolean`. Run the action; return
    /// true if it requests a halt (break).
    fn fire(&mut self, idx: usize, pc: u16, value: Option<u8>, addr: Option<u16>) -> bool {
        let action = self.observers[idx].action;
        match action {
            ObsAction::Break => true,
            ObsAction::Mark => {
                let label = {
                    let o = &self.observers[idx];
                    if o.mark_label.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
                        o.mark_label.clone().unwrap()
                    } else {
                        o.name.clone()
                    }
                };
                self.pending_marks.push(label);
                false
            }
            ObsAction::Cmd => {
                if let Some(cmd) = self.observers[idx].cmd_src.clone() {
                    self.pending_cmds.push(cmd);
                }
                false
            }
            ObsAction::Trace => {
                if let Some(ts) = self.observers[idx].trace_scope.clone() {
                    let name = self.observers[idx].name.clone();
                    self.pending_trace.push((ts.off, ts.domains, name));
                }
                false
            }
            ObsAction::Log => {
                let trigger = self.observers[idx].trigger;
                let where_str = if trigger == ObsTrigger::Exec {
                    format!("exec ${:04X}", pc)
                } else {
                    let t = match trigger {
                        ObsTrigger::Store => "store",
                        _ => "load",
                    };
                    format!("{t} ${:04X}=${:02X}", addr.unwrap_or(0), value.unwrap_or(0))
                };
                let fields = match self.observers[idx].log_exprs.clone() {
                    Some(exprs) if !exprs.is_empty() => self.render_log_exprs(&exprs, pc),
                    _ => format!("pc=${:04X} a=${:02X}", pc, self.env.a),
                };
                let name = self.observers[idx].name.clone();
                self.push_log(format!(
                    "obs {name}: {where_str}  {fields} cyc={}",
                    self.env.cy
                ));
                false
            }
        }
    }

    /// ts:264-268 — drain the queued side-effects.
    pub fn drain_pending_marks(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_marks)
    }
    pub fn drain_pending_cmds(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_cmds)
    }
    pub fn drain_pending_trace(&mut self) -> Vec<(bool, Vec<String>, String)> {
        std::mem::take(&mut self.pending_trace)
    }

    /// ts:271 — `renderLogExprs(exprs, pc): string`. Renders against the env
    /// snapshot + memory peeks (the daemon must set `env` first; mem peeks use a
    /// closure the caller wires when available — here we render against the
    /// snapshot registers and leave mem peeks as the byte the daemon resolves).
    fn render_log_exprs(&self, exprs: &[LogExpr], pc: u16) -> String {
        let e = self.env;
        exprs
            .iter()
            .map(|ex| match ex {
                LogExpr::Reg(RegName::Pc) => format!("pc=${:04X}", pc),
                LogExpr::Reg(RegName::A) => format!("a={:02X}", e.a),
                LogExpr::Reg(RegName::X) => format!("x={:02X}", e.x),
                LogExpr::Reg(RegName::Y) => format!("y={:02X}", e.y),
                LogExpr::Reg(RegName::Sp) => format!("sp={:02X}", e.sp),
                LogExpr::Reg(RegName::Fl) => format!("fl={:02X}", e.fl),
                LogExpr::Mem { addr, word } => {
                    // Mem peeks resolved by the daemon are not available inside the
                    // registry; the daemon-rendered path is used for live logs. Here
                    // we emit the address placeholder to keep the format stable.
                    if *word {
                        format!("${}=????", hx_addr(*addr))
                    } else {
                        format!("${}=??", hx_addr(*addr))
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// ts:290 — `pushLog(line)`.
    fn push_log(&mut self, line: String) {
        self.logs.push(line.clone());
        if self.logs.len() > 500 {
            let drop = self.logs.len() - 500;
            self.logs.drain(0..drop);
        }
        self.pending_log.push(line);
        if self.pending_log.len() > 500 {
            let drop = self.pending_log.len() - 500;
            self.pending_log.drain(0..drop);
        }
    }

    /// ts:298 — `drainPendingLog()`.
    pub fn drain_pending_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_log)
    }
}

/// ts:306-309 — hex helpers. Compact address: 2 hex digits for zero-page.
fn hx_addr(n: u16) -> String {
    if n < 0x100 {
        format!("{:02X}", n)
    } else {
        format!("{:04X}", n)
    }
}

/// The registry IS a core [`Observer`]: passed as `obs` to
/// `run_for_full_capped_dbg`, its `on_access` forwards the bus hook into the
/// policy. The other hooks are no-ops (the policy never needs the firehose).
impl CoreObserver for ObserverRegistry {
    #[allow(clippy::too_many_arguments)]
    fn on_instruction(
        &mut self,
        _pc: u16,
        _op: u8,
        _b1: u8,
        _b2: u8,
        _a: u8,
        _x: u8,
        _y: u8,
        _sp: u8,
        _p: u8,
        _clk: u64,
    ) {
    }
    fn on_bus(&mut self, _kind: BusKind, _addr: u16, _value: u8, _pc: u16, _clk: u64, _old: u8) {}
    fn on_interrupt(&mut self, _vector: u16, _clk: u64) {}
    fn on_access(&mut self, kind: BusKind, addr: u16, value: u8) -> bool {
        // The core only calls this when access_watch[addr] != 0 (the gate), so we
        // pay the policy eval ONLY on the exact watched addresses.
        self.on_access_policy(kind, addr, value);
        // Returning halt_requested tells the run loop to stop at the NEXT boundary.
        self.halt_requested
    }
}
