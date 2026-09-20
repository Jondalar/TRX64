//! Spec 864 §2 — the verbs.
//!
//! One implementation of what a monitor command means, against a host's machine. The
//! dispatch below sees every line first, because the monitor is modal: `a` swallows the
//! next line, an empty line leaves the mode, and a second copy of that knowledge is how
//! two hosts start behaving differently.
//!
//! [`try_exec`] answers `None` for a line this crate does not own, and the host's own
//! dispatch takes it from there. That is how the move stays honest while it is only
//! half done: every verb that has arrived here prints exactly what it printed in the
//! daemon, and the golden transcript is what says so.

use serde_json::Value;
use trx64_core::Machine;
use trx64_static::disasm6502::instr_len;

use crate::addr_spans::{self, Role as SpanRole, Space as SpanSpace};
use crate::assembler;
use crate::host::MonitorHost;
use crate::observers;
use crate::session::{sync_observers, BpEntry, MonitorSession, MonitorState, TrapRule};

/// reverse-debug Phase 2 — render a [`TriageChain`] as monitor text lines (the JAM
/// drop-in + the `triage` verb print this). The first line is the compact causal chain;
/// the rest break out the crash point, the wild transfer, and the corruptor slot(s) with
/// their confidence tags, so a low-confidence guess never looks like a pinned fact.
pub fn format_triage_lines(chain: &trx64_core::crash_triage::TriageChain) -> Vec<String> {
    use trx64_core::crash_triage::TransferKind;
    let mut lines = Vec::new();
    lines.push("── crash triage (reverse-debug Phase 2) ──────────────────────".to_string());
    // The compact one-line chain.
    lines.push(chain.summary.clone());
    // TRX64 feature-request #1 — the PINNED loop/halt onset, surfaced even after the
    // spin-storm evicted the entry transfer from the live history ring.
    if let Some(lo) = chain.loop_onset {
        lines.push(format!(
            "  loop entry: ${:04X} -> ${:04X} @cyc {}  (entered via op ${:02X}; A=${:02X} X=${:02X} Y=${:02X} SP=${:02X} P=${:02X})",
            lo.src_pc, lo.dst_pc, lo.cycle, lo.src_opcode, lo.a, lo.x, lo.y, lo.sp, lo.p
        ));
        lines.push(
            "            ↑ pinned at loop onset — survives the spin-storm that evicts the live ring."
                .to_string(),
        );
    }
    // Crash point + lead-in.
    lines.push(format!(
        "  crash:    JAM @ ${:04X}  op ${:02X}",
        chain.crash.pc, chain.crash.opcode
    ));
    if !chain.crash.lead_in.is_empty() {
        let trail: Vec<String> = chain
            .crash
            .lead_in
            .iter()
            .map(|e| format!("${:04X}", e.pc))
            .collect();
        lines.push(format!("  lead-in:  {}", trail.join(" → ")));
    }
    // The wild transfer.
    lines.push(format!(
        "  transfer: {} @ ${:04X} → ${:04X}  [{}]",
        chain.transfer.kind.as_str(),
        chain.transfer.at_pc,
        chain.transfer.landed_pc,
        chain.transfer.confidence.as_str()
    ));
    lines.push(format!("            {}", chain.transfer.note));
    // The corruptor slots (only present for a stack pop).
    if chain.transfer.kind.is_stack_pop() {
        for slot in &chain.corruptor_slots {
            if let (Some(pc), Some(cyc), Some(old), Some(new)) = (
                slot.writer_pc,
                slot.writer_cycle,
                slot.writer_old,
                slot.writer_new,
            ) {
                lines.push(format!(
                    "  slot ${:04X}=${:02X}  ← written by ${:04X} @ cyc {} (${:02X}→${:02X})  [{}]",
                    slot.addr, slot.value, pc, cyc, old, new, slot.confidence.as_str()
                ));
            } else {
                lines.push(format!(
                    "  slot ${:04X}=${:02X}  ← no writer in the live ring  [{}]",
                    slot.addr, slot.value, slot.confidence.as_str()
                ));
            }
        }
        if chain.pinned_corruptor {
            lines.push(
                "  ⇒ corruptor PINNED — the cited instruction put the bad byte on the stack."
                    .to_string(),
            );
        } else {
            lines.push(
                "  ⇒ corruptor NOT pinned (low confidence) — the bad return byte was stacked \
                 before the reverse window, or is a genuine return. Inspect manually."
                    .to_string(),
            );
        }
    } else if !matches!(chain.transfer.kind, TransferKind::Unknown) {
        lines.push(
            "  ⇒ not a stack smash — no stack corruptor to attribute (see the transfer note)."
                .to_string(),
        );
    }
    lines
}

/// TRX64 feature-request #4 — parse project-supplied on-trap dump rules from JSON. The
/// project file is either a single rule object or an array of them:
///   `{ "pc":"$088F", "label":"loader miss",
///      "dump":[["k1","$0A80",1],["k2","$0A81",1]],
///      "decode":"k2 bit7 => DIRECT-overlay miss" }`
/// Addresses/PC accept `$`-prefixed or bare hex; `len` defaults to 1 and clamps 1..=8.
/// Returns the parsed rules (NO built-in engine knowledge — the project owns the bytes)
/// or an error string describing the first malformed field.
pub fn parse_trap_rules(json: &Value) -> Result<Vec<TrapRule>, String> {
    let arr: Vec<&Value> = match json {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![json],
        _ => return Err("traprules: expected a JSON object or an array of objects".into()),
    };
    let hex16 = |v: &Value, field: &str| -> Result<u16, String> {
        let s = v.as_str().ok_or_else(|| format!("traprules: `{field}` must be a hex string"))?;
        parse_hex(s)
            .map(|n| (n & 0xffff) as u16)
            .ok_or_else(|| format!("traprules: `{field}`=\"{s}\" is not hex"))
    };
    let mut out = Vec::new();
    for (i, r) in arr.into_iter().enumerate() {
        let obj = r
            .as_object()
            .ok_or_else(|| format!("traprules: rule #{i} is not an object"))?;
        let pc = hex16(obj.get("pc").ok_or_else(|| format!("traprules: rule #{i} missing `pc`"))?, "pc")?;
        let label = obj
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("trap")
            .to_string();
        let decode = obj.get("decode").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mut dump = Vec::new();
        if let Some(d) = obj.get("dump") {
            let items = d
                .as_array()
                .ok_or_else(|| format!("traprules: rule #{i} `dump` must be an array"))?;
            for (j, item) in items.iter().enumerate() {
                let t = item
                    .as_array()
                    .ok_or_else(|| format!("traprules: rule #{i} dump[{j}] must be [name, addr, len?]"))?;
                let name = t
                    .first()
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("traprules: rule #{i} dump[{j}][0] (name) must be a string"))?
                    .to_string();
                let addr = hex16(
                    t.get(1).ok_or_else(|| format!("traprules: rule #{i} dump[{j}] missing addr"))?,
                    "dump addr",
                )?;
                let len = t
                    .get(2)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1)
                    .clamp(1, 8) as u8;
                dump.push((name, addr, len));
            }
        }
        out.push(TrapRule { pc, label, dump, decode });
    }
    Ok(out)
}

/// TRX64 feature-request #4 — render the diagnostic emit for a trap rule, reading the
/// `dump` bytes from the live machine (side-effect-free banked peek) and formatting
/// `label: name=$XX name2=$YYYY (decode)`. A multi-byte field is shown LE as one value.
/// Read-only.
pub fn format_trap_rule_emit(rule: &TrapRule, m: &trx64_core::Machine) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (name, addr, len) in &rule.dump {
        let mut val: u64 = 0;
        for k in 0..*len as u16 {
            let b = m.read_full(addr.wrapping_add(k)) as u64;
            val |= b << (8 * k); // little-endian
        }
        let width = (*len as usize) * 2;
        parts.push(format!("{name}=${val:0width$X}"));
    }
    let mut s = format!("{}: {}", rule.label, parts.join(" "));
    if !rule.decode.is_empty() {
        s.push_str(&format!(" ({})", rule.decode));
    }
    s
}

/// Spec 754 §3.3k — control-flow classification for the `df` static walk. 1:1 with
/// monitor-flow-disasm.ts `classify`: JMP abs / JMP (ind) / JSR / RTS / RTI / BRK /
/// conditional branch / normal. `target` carries the abs operand (or, for JMP(ind),
/// the POINTER address; for a branch, the resolved relative target).
pub enum CfKind {
    Normal,
    Jmp,
    JmpInd,
    Jsr,
    Rts,
    Rti,
    Brk,
    Branch,
}
pub struct CfInfo {
    pub size: u16,
    pub kind: CfKind,
    pub target: Option<u16>,
}
pub fn classify_cf(read: impl Fn(u16) -> u8, addr: u16) -> CfInfo {
    let op = read(addr);
    let size = instr_len(op) as u16;
    let abs = || -> u16 {
        (read(addr.wrapping_add(1)) as u16) | ((read(addr.wrapping_add(2)) as u16) << 8)
    };
    match op {
        0x4c => CfInfo { size, kind: CfKind::Jmp, target: Some(abs()) }, // JMP abs
        0x6c => CfInfo { size, kind: CfKind::JmpInd, target: Some(abs()) }, // JMP (ind)
        0x20 => CfInfo { size, kind: CfKind::Jsr, target: Some(abs()) }, // JSR abs
        0x60 => CfInfo { size, kind: CfKind::Rts, target: None },
        0x40 => CfInfo { size, kind: CfKind::Rti, target: None },
        0x00 => CfInfo { size, kind: CfKind::Brk, target: None },
        // Conditional branches BPL/BMI/BVC/BVS/BCC/BCS/BNE/BEQ.
        0x10 | 0x30 | 0x50 | 0x70 | 0x90 | 0xb0 | 0xd0 | 0xf0 => {
            let rel = read(addr.wrapping_add(1));
            let off = if rel < 0x80 { rel as i32 } else { rel as i32 - 256 };
            let target = ((addr as i32) + 2 + off) as u16;
            CfInfo { size, kind: CfKind::Branch, target: Some(target) }
        }
        _ => CfInfo { size, kind: CfKind::Normal, target: None },
    }
}

/// Screen-code → ASCII for the `screen` decode (display only). 1:1 with
/// monitor-shell.ts scToAscii: ignore the reverse-video bit, @ for 0, A-Z for 1-26,
/// space for 32, the punctuation/digit range 33-63 verbatim, '.' otherwise.
pub fn sc_to_ascii(sc: u8) -> char {
    let c = sc & 0x7f; // ignore the reverse-video bit
    if c == 0 {
        '@'
    } else if (1..=26).contains(&c) {
        (64 + c) as char // A-Z
    } else if c == 32 {
        ' '
    } else if (33..=63).contains(&c) {
        c as char // !"#…digits…?
    } else {
        '.'
    }
}

/// A monitor read through the bank lens, honouring the `sidefx` toggle.
///
/// Default (`sidefx off`) is the peek lane — looking must not change the machine. With
/// `sidefx on` the reference performs the LIVE read for the cpu/io lenses, so that
/// inspecting a register does to the machine exactly what the CPU would. That toggle was
/// stored and never consulted here, which made the monitor's own `sidefx = on (monitor
/// reads are LIVE — I/O side effects)` reply untrue.
pub fn monitor_read(mon: &MonitorState, m: &mut Machine, addr: u16, lens: &str) -> u8 {
    if mon.sidefx_on && matches!(lens, "cpu" | "io") {
        m.read_full_live(addr)
    } else {
        m.peek_lens(addr, lens)
    }
}

/// A monitor write through the bank lens. The WRITE is the library's — it is the
/// machine's own memory, and every host's machine is the same `trx64_core::Machine`.
/// What a host makes of it is the host's: the daemon latches the flags its
/// bus-selection gate reads, through [`MonitorHost::on_machine_write`].
pub fn monitor_write(host: &mut dyn MonitorHost, addr: u16, bytes: &[u8], lens: &str) {
    match lens {
        "ram" => {
            host.machine().poke(addr, bytes);
        }
        "io" => {
            host.machine().poke_io(addr, bytes);
        }
        _ => {
            let m = host.machine();
            for (i, b) in bytes.iter().enumerate() {
                m.write_full(addr.wrapping_add(i as u16), *b);
            }
        }
    }
    host.on_machine_write(lens);
}

/// T2.8 — the `help`/`?` text. VERBATIM copy of the monitor-shell.ts help block
/// (the help simply LISTS every verb of the VICE-superset, including ones whose
/// runtime bridges are deferred in this daemon — the help text itself is identical
/// regardless of which bridges are wired, so it is reproduced 1:1).
/// Anchored full-string glob match supporting `*` (any run, incl. empty) and `?`
/// (exactly one char) — the observer on/off/del wildcard (monitor-shell.ts:912-914
/// globMatches, where the pattern is `^` + escaped-name with `*`→".*", `?`→"."` +
/// `$`). Observer names are plain identifiers, so this character walk is equivalent.
pub fn glob_full_match(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    // Classic two-pointer glob match with backtracking over `*`.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn monitor_help_text() -> String {
    [
        "monitor (VICE-superset):",
        "  EXEC",
        "    g [addr]         go/resume the run-loop (PC=addr); Pause button halts",
        "    x                exit/resume (= g)",
        "    until <addr>     run until PC=addr, then stop (synchronous)",
        "    z / step         step into — may enter IRQ/NMI (VICE-correct)",
        "    n / next         step over — skips JSR + runs THROUGH IRQ/NMI",
        "    ret / return     run until current frame returns (RTS/RTI)",
        "    focus [m]        flow focus: auto|main|irq|nmi|brk|clear (C64RE)",
        "    sf / nf          step into/over, stop only in focused flow (C64RE)",
        "    flow             interrupt/trap flow frame stack (panel)",
        "    bt               backtrace (stack scan + flow frames)",
        "    reset            cold reset",
        "  MEMORY (bank lens: cpu|ram|rom|io|cart, default cpu = what CPU sees)",
        "    m [lens] <a> [b] memory dump ($20/row + petscii; default len $800)",
        "    d [lens] [a] [end] disassemble: a..end range (VICE), or ~16 from a/PC",
        "    sd [n]           step+disasm: the REAL executed path, loops folded (dynamic)",
        "    df [-i] [a] [n]  follow-disasm: walk control flow (static); -i asks at branches (df t|f|b)",
        "    screen           decode the 40x25 text screen (real screen pointer)",
        "    io [1|addr]      I/O area per device: register hex (peek) + state details (VICE io)",
        "    iec              the serial bus: ATN/CLK/DATA, their level, and WHICH side is pulling each one low, plus both ends as their CPUs see them ($DD00 and the 1541's $1800). A released line is high and any device may pull it low, so \"who is low\" is per-device, not a bus-wide state. Answers the only question an IEC stall ever asks.",
        "    bitmap <a> [w h] [hires|charset|sprite]  render a RAM range to a PNG (scrub gfx)",
        "    bank [lens]      show/set the sticky default lens for m/d",
        "    wr [lens] <a> <b..>  write exactly these bytes from a",
        "    f <a> <b> <d..>  fill range a..b with repeating data",
        "    a <a> [instr]    assemble; `a c000` enters assemble mode (type lines, empty exits)",
        "    t <a> <b> <dst>  move/copy a..b to dst (overlap-safe)",
        "    c <a> <b> <dst>  compare a..b vs dst (list diffs)",
        "    h <a> <b> <d..>  hunt for a byte pattern (xx = wildcard)",
        "  BREAKPOINTS / OBSERVERS",
        "    bk               list breakpoints (#num $addr)",
        "    bk <a> | bk -<a> set / remove breakpoint (by addr)",
        "    del <n..> | del  delete by #num / delete all",
        "    obs <name> when exec|load|store <a[..b]> [if <cond>] do <action> [fields]",
        "      actions: break | log [fields] | mark [\"label\"] | cmd \"<cmd>\" | trace [domains]|off",
        "      log fields: a/x/y/sp/pc/fl or $addr[:w]  e.g. `do log $fd $fe $ff a x y`",
        "      trace domains: c64-cpu|drive8-cpu|iec|vic|memory (default c64-cpu+memory)",
        "        bracket: `obs c when exec $4000 do trace` … `obs c2 when exec $4100 do trace off`",
        "    obs | obs log    list observers / show log lines",
        "    obs <name> on|off|del   (name may glob: `obs * del` = all, `obs c* off`)",
        "    ignore <name> [n]",
        "      cond: a/x/y/pc/sp/fl/rl/val/addr  == != < > <= >= && || ( )",
        "  CPU",
        "    r                registers (+ flow + IRQ/NMI vectors)",
        "    r a=$42 x=$10    set registers (a/x/y/sp/pc/fl)",
        "    sidefx [on|off]  monitor read side effects (default off = peek)",
        "    device [c64|drive8]  target the C64 or the 1541 CPU (drive8 = read-inspect r/m/d)",
        "  STATE / TRACE",
        "    dump|snapshot <p>  write a .c64re snapshot; undump|loadsnapshot <p>  restore it",
        "    savecrt [\"<p>\"]  write live flash state to the mounted .crt (or to <p> as a copy)",
        "    swapcrt \"<p>\"    hot-swap the .crt, NO reset (same mapper: bank/ctrl carried) — build iteration",
        "    trace on|off|status|mark   live trace gate",
        "    tracedb start|stop|status|mark   declarative trace",
        "    traceindex [path]   build the .duckdb index for the current/last (or <path>) .c64retrace so it is queryable (oldest->newest, no event cap)",
        "    tracering <s> <e> [path]  build a .c64retrace from the ALWAYS-ON reverse ring, AFTER the fact — the window you did not arm a trace for. `revdepth` says how far back the ring reaches; a start older than that silently begins where the ring does.",
        "  ANALYSIS (need a trace — `trace on` first)",
        "    map [cpu]        memory map: free RAM / persistence surface",
        "    taint <a> [cyc]  data-flow taint backward from (cyc,addr)",
        "    swimlane [list|name] [s] [e]  trace lanes (cpu/irq/nmi/io/1541): list / newest / by name; tail ~2000cy",
        "                     `swimlane <s> <e>` with no covering trace → auto checkpoint-ring replay",
        "    chis [cyc] | chis <s> <e>  cpu instruction history: LIVE cpuhistory ring first (works while a trace is active), falls back to the captured trace; last N cyc (default 4000) or a window",
        "  REVERSE-DEBUG (always-on full-delta ring — no pre-arming; inspect-backward only)",
        "    rstep [n] | reverse [n]   UNDO the last n instructions (default 1): restore CPU+RAM+IO bytes to before them; reports the landed regs + writes rolled back",
        "    whowrote <addr> [n]       last n writer(s) of <addr> from the ring (newest first): PC + cycle + old->new + caller chain (top return frames -> identifies the CALLER of a shared store). Emits `ring_exhausted: true` + a depth hint on a miss past a wrapped ring.",
        "    triage [pc]               guided crash-triage: causal chain (crash -> wild RTS/JMP transfer -> stack corruptor) from the rings; auto-printed on a JAM. Confidence-tagged. Surfaces a PINNED `loop entry: $SRC -> $DST` for a tight-loop/halt; `ring_exhausted` when the transfer is older than the ring.",
        "    traprules <path> | traprules [clear]   load/list/clear project on-trap dump rules (JSON {pc,label,dump:[[name,addr,len]],decode}); auto-emits `label: name=$XX (decode)` on reaching that PC (JAM / breakpoint)",
        "    revdepth [seconds]        report / set the always-on reverse-ring depth: rebuilds the delta+cpuhistory rings (DISCARDS history; future capture only; 1..=600s). TRX64_REVERSE_SECONDS = boot default",
        "    diff <idA> <idB>          typed by-ID diff of two checkpoint anchors (RAM runs + per-chip register changes). READ-ONLY (live machine unchanged). ids from `checkpoint/list`",
        "  MEDIA + DRIVE (Spec 839 — the same verbs on every front-end; the cockpit used to own these)",
        "    mount <path>              put a .d64/.g64/.crt/.prg/.c64re in the machine. The TYPE comes from the file's CONTENT, not its extension; a relative path resolves against `pwd`/`cd`. A cartridge power-cycles, a disk does not.",
        "    eject [cart|disk]         take it out. Bare `eject` targets whatever is actually in (cartridge first, else the disk). Both persist to the host file FIRST — a disk eject leaves the drive turning, a cartridge eject cold-resets the machine (that is what pulling a cart does).",
        "    drive                     drive 8 live status: motor, track, LED, what is mounted, whether it is dirty",
        "    cart                      cartridge live status: type, bank, read/write activity — null when nothing is inserted",
        "    drivepower                cold-reset the drive 6502 ONLY (DOS re-runs power-on init). The C64 side is untouched. Every scrap of drive-side state goes: open channels, a fastloader's uploaded drivecode, a half-written sector. It is the way out of a wedged fastloader without power-cycling the machine someone is watching.",
        "    recent                    the media this daemon has had mounted lately (daemon state, not project history)",
        "  MACHINE (the same verbs on every front-end — the cockpit\'s `/` prefix is input sugar)",
        "    run                       resume the machine (from a rewound point: cuts the anchors ahead)",
        "    pause                     stop the machine AND the transport; prints the ringbuffer range",
        "    warp on|off               8\u{00d7} pacing / real time (the model's frame rate)",
        "    rawframe on|off           an anchor stores no picture, so stepping onto one REDRAWS it: two frames, keep the second, because the first is cut into wherever the anchor landed in the raster. That discards a ONE-FRAME event — a border opened for a frame, a bad raster split — so it looks like the ring never caught it. `on` keeps the FIRST frame, seam and all. Anchors that sit on a frame boundary use the first either way (whole, nothing lost). `transport/status` reports which you are looking at as `shownFrame`.",
        "    reset [warm|cold] · power on|off",
        "    model                     which C64 this is (model, video standard, VIC-II, frame, clock), and every model this build knows — with what a model that cannot run is missing",
        "    model <row>               switch the running machine to another model (c64-pal, c64-ntsc, c64-paln …) at the next frame boundary. Not a power cycle: the program keeps its state and the standard it detected at boot; `reset` or `power off`/`on` afterwards for a clean start on the new model. The model survives reset and power cycles.",
        "    turbo                     Spec 815 — which machine this session CLAIMS to be, so a release's turbo code path is reachable at all. A C64 answers $FF at $D02F-$D03F, the probe fails, and everything behind it is dead code.",
        "    turbo mode c64|128|u64    c64 (default) = open bus. 128 = the VIC-IIe pair $D02F/$D030 with VICE's read-back masks. u64 = an extended speed register at $D031. Survives a reset: it is machine identity, not chip state.",
        "    turbo on|off              set/clear the speed bit the way the release would ($D030 bit 0, or $D031)",
        "    turbo speed $NN           the extended speed value (u64 profile)",
        "                              The speed bit is STORED, not acted on: the CPU still runs at 1 MHz and the picture is unchanged. What a set bit does to the display is Spec 815 §3 and is unbuilt on purpose — guessing it would put behaviour here that exists nowhere else.",
        "    uci                       Spec 852 — the Ultimate Command Interface on the u64 profile, read-only: enabled, window, state, pointers, lengths, the IRQ and freeze lines, events the firmware has not taken. Disabled without a firmware, so the window reads open bus.",
        "  MARKS (Spec 809 — a named, pinned point you can iterate FROM)",
        "    mark <name>               name + pin the anchor you are standing on (max 32)",
        "    marks                     list them with cycle, frame, how far back, and the window cost",
        "    unmark <name>             drop the name and the pin",
        "    goto <name>               jump to a mark (goto also takes a frame or c<cycle>)",
        "    A mark survives PLAY cutting the future, which is what lets you go there, try",
        "    something, come back and try differently. `ringdump` carries marks, so a",
        "    .c64rering is a session WITH its bookmarks.",
        "    identify <path>           what a file IS, from its content: c64re|crt|g64|d64|prg (+ whether a PRG would autostart)",
        "  REWIND TRANSPORT (Spec 808 — plays the MACHINE backwards, not cached pictures)",
        "    play back|fwd [speed]     play through the anchors; every step is a real restore, so registers/memory/drive are correct at every frame. `play fwd` at the head just runs.",
        "    pause                     stop where you are — the machine IS there, no second step needed",
        "    frame -N | +N             step N anchors (stops at the ends, never wraps)",
        "    goto <frame> | goto c<cyc>  jump to a position; a cycle lands on the anchor at-or-before it",
        "    rewind                    mode + position + window + anchors held, and the key legend",
        "    keys: F9 one back | F10 play back | F11 pause/play | F12 one forward",
        "    Watching is free: replaying keeps the anchors. Only an INTERVENTION (a write, a key, a resumed run) truncates the future — the mode line then says CUT and how many anchors went.",
        "  CHECKPOINT RING (the scrub/filmstrip buffer — full machine states, NOT the delta ring)",
        "    cadence                   report the capture rate, window, entry cap and how many anchors are held",
        "    window [seconds]          how far back you want to reach (default 60s, max 600) — recomputes the cap AND the byte budget",
        "    cadence <frames> [secs]   set the rate AND retune the cap together (`cadence 1` = one full checkpoint per frame). States the expected memory (~98 KiB/anchor) before spending it. 1..=3000 frames, window 1..=600s",
        "    ringdump <path>           serialize the WHOLE reverse-debug buffer (checkpoint+delta+cpuhistory rings) → one gzipped .c64rering file (the tester->dev hand-off)",
        "    ringload <path>           restore a .c64rering: reconstruct the rings + restore the machine to its current anchor; scrub/rstep/whowrote/chis/diff then work on it",
        "  NAMES — none here: TRX64 holds no symbols. Every monitor/exec reply says WHERE it",
        "  printed each address (spans: line, column, address, space, role); C64RE names them.",
        "  FILE (rooted at the project dir; relative paths off the session cwd)",
        "    pwd | cd [dir] | ls [dir]   FS shell (cd with no arg = project dir)",
        "    mkdir <dir> | rmdir <dir>   make / remove a directory",
        "    load \"<f>\" [addr]   load a PRG into RAM (2-byte header, or override addr)",
        "    save \"<f>\" <a1> <a2>  save a1..a2 as a PRG (2-byte load addr = a1)",
        "    bload \"<f>\" <addr>   raw binary load (no header)",
        "    bsave \"<f>\" <a1> <a2>  raw binary save (no header)",
    ]
    .join("\n")
}

/// The first double-quoted substring of a command (= the TS
/// `[...cmd.matchAll(/"([^"]*)"/g)].map(m => m[1])[0]`). `None` when unquoted.
/// Render the `do <action>` description of an observer spec for the `obs`/`o`/`reg`
/// list + the registration echo — 1:1 with the c64re `doDesc` (monitor-shell.ts:
/// 996-1000) and the `fmt` `do ...` segment (monitor-shell.ts:899).
pub fn obs_do_desc(spec: &observers::ObsSpec) -> String {
    match spec.action {
        observers::ObsAction::Log => match &spec.log_exprs {
            Some(exprs) if !exprs.is_empty() => {
                let fields: Vec<String> = exprs
                    .iter()
                    .map(|e| match e {
                        observers::LogExpr::Reg(r) => match r {
                            observers::RegName::A => "a".into(),
                            observers::RegName::X => "x".into(),
                            observers::RegName::Y => "y".into(),
                            observers::RegName::Sp => "sp".into(),
                            observers::RegName::Pc => "pc".into(),
                            observers::RegName::Fl => "fl".into(),
                        },
                        observers::LogExpr::Mem { addr, word } => {
                            format!("${:x}{}", addr, if *word { ":w" } else { "" })
                        }
                    })
                    .collect();
                format!("log {}", fields.join(" "))
            }
            _ => "log".into(),
        },
        observers::ObsAction::Cmd => {
            format!("cmd \"{}\"", spec.cmd_src.clone().unwrap_or_default())
        }
        observers::ObsAction::Mark => {
            format!("mark \"{}\"", spec.mark_label.clone().unwrap_or_default())
        }
        observers::ObsAction::Trace => match &spec.trace_scope {
            Some(ts) if ts.off => "trace off".into(),
            Some(ts) => format!("trace {}", ts.domains.join(" ")),
            None => "trace".into(),
        },
        observers::ObsAction::Break => "break".into(),
    }
}

pub fn quoted_first(cmd: &str) -> Option<String> {
    let start = cmd.find('"')?;
    let rest = &cmd[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Modal assemble prompt at `addr` (= monitor-shell.ts `asmPrompt`): VICE-style
/// `.cXXXX  ` (dot, lower-case 4-hex, two trailing spaces).
pub fn asm_prompt(addr: u16) -> String {
    format!(".{:04x}  ", addr & 0xffff)
}

/// Assemble one instruction at `addr` and write it (= monitor-shell.ts `assembleAt`,
/// :191-204). On success: poke the bytes via the CPU write path, advance BOTH the
/// modal assemble cursor and the disasm cursor (→ stay in mode), set `pending_prompt`
/// to the next prompt, and return the `addr  bb bb  <disasm>` listing line. On error:
/// return the error (cursor unchanged; the caller re-shows the prompt). The bytes are
/// written through `poke` (raw RAM), matching the TS `s.c64Bus.write` for RAM targets;
/// the disassembly read-back uses the cpu lens, 1:1 with the TS `disasmLine(peek cpu)`.
pub fn assemble_at(
    mon: &mut MonitorSession,
    host: &mut dyn MonitorHost,
    addr: u16,
    text: &str,
) -> Result<String, String> {
    let r = assembler::assemble_line(text, addr).map_err(|e| format!("a: {e}"))?;
    host.machine().poke(addr, &r.bytes);
    host.on_machine_write("ram");
    let next = addr.wrapping_add(r.size);
    mon.state.asm_cursor = Some(next);
    mon.state.disasm_cursor = Some(next);
    mon.state.pending_prompt = Some(asm_prompt(next));
    let bytes_col: String = r.bytes.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
    let m = host.machine();
    let (_, back) = addr_spans::disasm_line(|a| m.peek_lens(a, "cpu"), addr, SpanSpace::C64);
    Ok(format!("{:04x}  {:<11}  {}", addr, bytes_col, back))
}

/// Parse a hex token (optional leading `$`).
pub fn parse_hex(tok: &str) -> Option<u32> {
    let t = tok.strip_prefix('$').unwrap_or(tok);
    u32::from_str_radix(t, 16).ok()
}

/// Map a TRX64 [`trx64_core::cart::MapperType`] to the c64re
/// HeadlessCartridgeMapperType string (cartridge.ts) the cart_status `type` field
/// carries, so the wire value matches the TS daemon.
pub fn mapper_type_str(t: trx64_core::cart::MapperType) -> &'static str {
    use trx64_core::cart::MapperType::*;
    match t {
        Normal8k => "normal_8k",
        Normal16k => "normal_16k",
        Ultimax => "ultimax",
        Ocean => "ocean",
        MagicDesk => "magicdesk",
        MagicDesk16 => "magicdesk16",
        EasyFlash => "easyflash",
        EasyFlashXl => "easyflash_xl",
        Gmod2 => "gmod2",
        MegaByter => "megabyter",
        C64MegaCart => "c64megacart",
        Gmod4 => "gmod4",
        // Spec 790 S2 — the self-configuring harness before it locks a concrete
        // family (post-lock it delegates `mapper_type()` and never returns this).
        SelfConfig => "self_config",
        Unsupported => "cartridge",
    }
}

/// The verbs this crate owns today. A line whose verb is not here falls through to the
/// host's own dispatch — the honest shape while the move is half done, and the shape
/// §6 keeps afterwards for a host's own verbs.
const OWNED: [&str; 35] = [
    "r",
    "registers",
    "wr",
    "m",
    "mem",
    "d",
    "disass",
    "screen",
    "f",
    "fill",
    "a",
    "t",
    "move",
    "c",
    "compare",
    "h",
    "hunt",
    "bank",
    "sidefx",
    "obs",
    "o",
    "ignore",
    "bk",
    "break",
    "b",
    "del",
    "delete",
    "flow",
    "io",
    "iec",
    "focus",
    "bt",
    "triage",
    "help",
    "?",
];

/// Run one monitor line against a host.
///
/// `None` means "not a verb this crate owns"; the host answers it. Everything before
/// that point — the prompt, the assemble mode, the selected device — is decided HERE,
/// because it is what makes the monitor modal, and a second copy of it is how two hosts
/// start behaving differently.
pub fn try_exec(
    mon: &mut MonitorSession,
    host: &mut dyn MonitorHost,
    command: &str,
) -> Option<Result<String, String>> {
    // Clear any prompt carried from a prior command; a modal verb re-sets it below.
    mon.state.pending_prompt = None;
    let cmd = command.trim().to_string();

    // ---- Modal assemble interception (Spec 754 §3.3c). 1:1 with monitor-shell.ts
    // :218-223. A session in assemble mode treats EVERY line as an instruction (no
    // verb dispatch); an empty line EXITS. A bad instruction stays in mode + re-shows
    // the prompt (friendlier than VICE, which silently drops out — intentional). This
    // runs BEFORE the empty-line no-op below because in mode an empty line is the
    // explicit exit, not a no-op.
    if let Some(at) = mon.state.asm_cursor {
        if cmd.is_empty() {
            mon.state.asm_cursor = None;
            return Some(Ok(String::new()));
        }
        match assemble_at(mon, host, at, &cmd) {
            Ok(out) => return Some(Ok(out)),
            Err(e) => {
                // Re-show the prompt at the UNCHANGED cursor (cursor not advanced).
                mon.state.pending_prompt = Some(asm_prompt(at));
                return Some(Err(e));
            }
        }
    }

    if cmd.is_empty() {
        return Some(Ok(String::new()));
    }
    let toks: Vec<String> = cmd.split_whitespace().map(|s| s.to_string()).collect();
    let op = toks[0].to_ascii_lowercase();

    // readByte/writeByte (= monitor-shell readByte/writeByte). When device=drive8
    // the read-inspect verbs r/m/d peek the 1541 drive CPU address space
    // (drive_peek); the C64 path is unchanged otherwise.
    // (closures borrow the machine; defined per-branch to satisfy the borrow checker)
    let device = mon.state.device.clone();

    // ---- device target (Spec 754 §3.3i / audit ws-trace-monitor-misc-8) ----------
    // Sticky inspect target: `device` shows / `device c64|drive8` sets. While
    // device=drive8 the monitor is READ-INSPECT only — only r/m/d (+ device/help)
    // act on the 1541 CPU; every other verb is blocked with a clear message (so it
    // can't silently mutate the C64). 1:1 with monitor-shell.ts:233-245.
    if op == "device" || op == "dev" {
        let arg = toks.get(1).map(|s| s.to_ascii_lowercase()).unwrap_or_default();
        if arg.is_empty() {
            return Some(Ok(format!(
                "device: {device}   (c64 | drive8 — drive8 = read-inspect r/m/d on the 1541 CPU)"
            )));
        }
        if arg == "c64" || arg == "drive8" {
            mon.state.device = arg.clone();
            return Some(Ok(format!("device: {arg}")));
        }
        return Some(Err("device: usage: device c64|drive8".into()));
    }
    // Spec 754 §3.3i — drive8 is read-inspect only: allow r/m/d (+ help/?). Anything
    // else would act on the C64 → block it (matches monitor-shell.ts:243).
    if device == "drive8" && !matches!(op.as_str(), "r" | "m" | "d" | "help" | "?") {
        return Some(Err(format!(
            "device drive8: read-inspect only (r/m/d). `device c64` first to use `{op}`."
        )));
    }


    if !OWNED.contains(&op.as_str()) {
        return None;
    }
    Some(exec_owned(mon, host, &cmd, &toks, &op))
}

fn exec_owned(
    mon: &mut MonitorSession,
    host: &mut dyn MonitorHost,
    cmd: &str,
    toks: &[String],
    op: &str,
) -> Result<String, String> {
    // --- TS-local helpers (closures over no state — pure parsers/formatters). ---
    // parseAddr: hex with optional `$`, masked to 16 bits; None on non-hex.
    let parse_addr = |t: Option<&String>| -> Option<u16> {
        t.and_then(|t| parse_hex(t)).map(|v| (v & 0xffff) as u16)
    };
    // parseByte: hex $00-$FF; None if out of range / non-hex.
    let parse_byte = |t: Option<&String>| -> Option<u8> {
        t.and_then(|t| parse_hex(t)).and_then(|v| if v <= 0xff { Some(v as u8) } else { None })
    };
    const LENSES: [&str; 5] = ["cpu", "ram", "rom", "io", "cart"];
    // lensOf: a bank word; `default` → the sticky default. None if absent/other.
    let bank_default = mon.state.bank_default.clone();
    let lens_of = |t: Option<&String>| -> Option<String> {
        let t = t?;
        let l = t.to_ascii_lowercase();
        if l == "default" {
            return Some(bank_default.clone());
        }
        if LENSES.contains(&l.as_str()) { Some(l) } else { None }
    };

    // sidefx OFF (default) → side-effect-free peek; ON → the LIVE read the CPU would
    // do, via `monitor_read` → `Machine::read_full_live`. The toggle used to be stored
    // and never consulted, so the monitor answered "reads are LIVE" while every read
    // stayed a peek. It covers the inspection verbs (m/c/t/h); `d`/`sd`/`bitmap` render
    // through a plain `Fn(u16) -> u8` and stay peeks, which is also the sane reading —
    // a listing should not alter the machine it describes.


    let device = mon.state.device.clone();

    match op {
        "r" | "registers" => {
            let flow_now = mon.flow.current_flow();
            // audit ws-trace-monitor-misc-8 — device drive8: the 1541 CPU registers
            // (read-only). 1:1 with monitor-shell.ts:481-488 (drive_pc / a / x / y / sp
            // / flags / drive_clk + track/halftrack), so the panel is unambiguously the
            // DRIVE core (header "1541 (drive 8)"), distinct from the C64 panel.
            if device == "drive8" {
                let drv = &host.machine().drive8;
                let c = &drv.core;
                let flags = c.status();
                let names = ['N', 'V', '-', 'B', 'D', 'I', 'Z', 'C'];
                let flags_str: String = names
                    .iter()
                    .enumerate()
                    .map(|(i, &f)| {
                        if (flags >> (7 - i)) & 1 != 0 { f } else { f.to_ascii_lowercase() }
                    })
                    .collect();
                let led = drv.led_on();
                let halftrack = drv.rotation.current_half_track;
                // NO `+ 1`: the field's own doc says "current_half_track (2..=84).
                // Power-on 36 (T18)", so half-track 36 IS track 18. The extra one showed
                // the head a whole track further out than it was.
                let track = halftrack / 2;
                return Ok(format!(
                    "1541 (drive 8)\n  \
                     ADDR AC XR YR SP NV-BDIZC  clk\n\
                     .;{} {:02x} {:02x} {:02x} {:02x} {}  {}\n  \
                     track {} (halftrack {})  led {}",
                    addr_spans::mark(&format!("{:04x}", c.reg_pc), c.reg_pc, SpanSpace::Drive8, SpanRole::Pc, None, 1),
                    c.reg_a, c.reg_x, c.reg_y, c.reg_sp, flags_str, drv.drive_clk,
                    track, halftrack,
                    if led { "on" } else { "off" }
                ));
            }
            let sets: Vec<&String> = toks[1..].iter().filter(|t| t.contains('=')).collect();
            if !sets.is_empty() {
                let mut done = Vec::new();
                for pair in sets {
                    let mut it = pair.splitn(2, '=');
                    let reg = it.next().unwrap_or("").to_ascii_lowercase();
                    let val_s = it.next().unwrap_or("");
                    let v = match parse_hex(val_s) {
                        Some(v) => v,
                        None => {
                            done.push(format!("bad {pair}"));
                            continue;
                        }
                    };
                    let c = &mut host.machine().cpu6510;
                    match reg.as_str() {
                        "a" | "ac" => { c.reg_a = v as u8; done.push(format!("a=${:02x}", v as u8)); }
                        "x" | "xr" => { c.reg_x = v as u8; done.push(format!("x=${:02x}", v as u8)); }
                        "y" | "yr" => { c.reg_y = v as u8; done.push(format!("y=${:02x}", v as u8)); }
                        "sp" => { c.reg_sp = v as u8; done.push(format!("sp=${:02x}", v as u8)); }
                        "pc" => {
                            c.reg_pc = v as u16;
                            // Also drive the live full-machine core's PC, so a subsequent
                            // `g`/`step` resumes from here on the full-machine path (the TS
                            // `r pc=` sets the one CPU; TRX64 has two cores kept in sync).
                            host.machine().c64_core.reg_pc = v as u16;
                            mon.state.disasm_cursor = Some(v as u16);
                            done.push(format!("pc=${:04x}", v as u16));
                        }
                        "p" | "fl" | "flags" => {
                            c.reg_p = (v as u8) & !0xa2;
                            c.flag_n = (v as u8) & 0x80;
                            c.flag_z = if (v as u8) & 0x02 != 0 { 0 } else { 1 };
                            done.push(format!("fl=${:02x}", v as u8));
                        }
                        _ => done.push(format!("unknown reg '{reg}'")),
                    }
                }
                host.machine().sync_after_monitor();
                host.on_machine_write("cpu");
                Ok(format!("set {}", done.join(" ")))
            } else {
                // Registers view (Spec 754 §3.3d, variant B): the VICE register line
                // with the flow column, then a PLA-port line and an IRQ/NMI vectors
                // line. 1:1 with monitor-shell.ts `r`. TRX64 has no FlowTracker, so
                // `flow` is reported as MAIN (the common post-boot case — no fabricated
                // interrupt frame; an honest constant, not a faked stack).
                let m = &host.machine();
                let c = &m.cpu6510;
                // flags string: NV-BDIZC, upper if set / lower if clear (= disasmLine).
                let flags = c.flags();
                let names = ['N', 'V', '-', 'B', 'D', 'I', 'Z', 'C'];
                let flags_str: String = names
                    .iter()
                    .enumerate()
                    .map(|(i, &f)| {
                        if (flags >> (7 - i)) & 1 != 0 { f } else { f.to_ascii_lowercase() }
                    })
                    .collect();
                // Vectors via the cpu lens (KERNAL banked) — peek, no side effect.
                let pk = |a: u16| m.peek_lens(a, "cpu");
                let w16 = |lo: u16, hi: u16| (pk(lo) as u16) | ((pk(hi) as u16) << 8);
                let irq_hw = w16(0xfffe, 0xffff);
                let nmi_hw = w16(0xfffa, 0xfffb);
                let cinv = w16(0x0314, 0x0315);
                let nmiv = w16(0x0318, 0x0319);
                // PLA banking latches: $00 = direction, $01 = port value (low 3 bits
                // select LORAM/HIRAM/CHAREN).
                let ddr = m.port_dir;
                let port = m.port_data;
                let loram = port & 1;
                let hiram = (port >> 1) & 1;
                let charen = (port >> 2) & 1;
                // The flow column was the literal string "MAIN", so the panel claimed
                // main flow while the CPU sat inside an interrupt handler — the one place
                // you look to find out. The tracker is right there and `flow`/`bt`
                // already read it.
                let flow = flow_now.tag().to_ascii_uppercase();
                // Spec 804 — the PC and the vectors' contents are marked, so a client can
                // name them without reading the panel's columns.
                let (c64, pc_role, tgt, memr) = (SpanSpace::C64, SpanRole::Pc, SpanRole::Target, SpanRole::Memory);
                Ok(format!(
                    "  ADDR AC XR YR SP NV-BDIZC  flow\n\
                     .;{} {:02x} {:02x} {:02x} {:02x} {}  {}\n  \
                     port  $00=${:02x} $01=${:02x}  LORAM={} HIRAM={} CHAREN={}\n  \
                     vectors  IRQ hw={}  CINV {}->{}     NMI hw={}  NMIV {}->{}",
                    addr_spans::mark(&format!("{:04x}", c.reg_pc), c.reg_pc, c64, pc_role, None, 1),
                    c.reg_a, c.reg_x, c.reg_y, c.reg_sp, flags_str, flow,
                    ddr, port, loram, hiram, charen,
                    addr_spans::addr4(irq_hw, c64, tgt),
                    addr_spans::addr4(0x0314, c64, memr),
                    addr_spans::addr4(cinv, c64, tgt),
                    addr_spans::addr4(nmi_hw, c64, tgt),
                    addr_spans::addr4(0x0318, c64, memr),
                    addr_spans::addr4(nmiv, c64, tgt)
                ))
            }
        }

        // ---- Memory edit: wr [lens] <addr> <byte..> --------------------------
        "wr" => {
            let mut i = 1;
            // All five bank words plus `default`, via the same `lens_of` the read verbs
            // use. Recognising only cpu/ram/io meant `wr rom c000 ea` parsed "rom" as the
            // ADDRESS and failed with "bad address", and the sticky `bank <lens>` default
            // had no effect on writes at all.
            let lens_tok = lens_of(toks.get(i));
            let lens = lens_tok.clone().unwrap_or_else(|| mon.state.bank_default.clone());
            if lens_tok.is_some() {
                i += 1;
            }
            let addr = parse_addr(toks.get(i)).ok_or("wr: usage: wr [lens] <addr> <byte..>")? as u16;
            i += 1;
            let bytes: Result<Vec<u8>, String> = toks[i..]
                .iter()
                .map(|t| parse_byte(Some(t)).ok_or_else(|| "wr: need >=1 byte value ($00-$FF)".to_string()))
                .collect();
            let bytes = bytes?;
            if bytes.is_empty() {
                return Err("wr: need >=1 byte value ($00-$FF)".into());
            }
            monitor_write(host, addr, &bytes, &lens);
            Ok(format!("wrote {} byte(s) @ ${addr:04x} ({lens})", bytes.len()))
        }

        // ---- Memory dump: m [lens] [addr] [end] (§3.3b bank lens). -----------
        // $20 bytes/row + PETSCII column, default length $800. peek (no side fx).
        "m" | "mem" => {
            let mut i = 1;
            let lens_tok = lens_of(toks.get(i));
            let lens = lens_tok.clone().unwrap_or_else(|| mon.state.bank_default.clone());
            if lens_tok.is_some() {
                i += 1;
            }
            // A token that is PRESENT but unparseable must not fall through to the
            // cursor: `m 1000xyz` then dumped some unrelated region and called it a
            // success. Absent → cursor (the documented default); present-and-bad → say so.
            let start = match toks.get(i) {
                Some(t) => parse_addr(Some(t)).ok_or_else(|| format!("m: bad address '{t}'"))?,
                None => mon.state.mem_cursor.unwrap_or(0),
            };
            let end = parse_addr(toks.get(i + 1))
                .unwrap_or_else(|| std::cmp::min(0xffff, start as u32 + 0x7ff) as u16);
            let mut lines: Vec<String> = Vec::new();
            // for (a = start & ~0x1f; a <= end; a += 32)
            let mut a: u32 = (start & !0x1f) as u32;
            let end_u = end as u32;
            while a <= end_u {
                let mut bytes: Vec<String> = Vec::new();
                let mut ascii = String::new();
                let mut row_len: u16 = 0;
                for j in 0..32u32 {
                    let aj = a + j;
                    if aj > end_u {
                        break;
                    }
                    // device drive8: peek the 1541 CPU address space (read-inspect),
                    // else the C64 banked lens (monitor-shell.ts:150-156 driveProbe).
                    let b = if device == "drive8" {
                        host.machine().drive8.drive_peek((aj & 0xffff) as u16)
                    } else {
                        monitor_read(&mon.state, host.machine(), (aj & 0xffff) as u16, &lens)
                    };
                    bytes.push(format!("{:02x}", b));
                    ascii.push(if (0x20..0x7f).contains(&b) { b as char } else { '.' });
                    row_len += 1;
                }
                let lens_letter = if lens == "cpu" {
                    'C'
                } else {
                    lens.chars().next().unwrap().to_ascii_uppercase()
                };
                // Spec 804 — the row address is a RANGE span: the row shows `row_len`
                // bytes from it, read through `lens` (the drive has no lens).
                let row_addr = (a & 0xffff) as u16;
                let row_mark = if device == "drive8" {
                    addr_spans::mark(&format!("{row_addr:04x}"), row_addr, SpanSpace::Drive8, SpanRole::Memory, None, row_len)
                } else {
                    addr_spans::mark(&format!("{row_addr:04x}"), row_addr, SpanSpace::C64, SpanRole::Memory, Some(&lens), row_len)
                };
                lines.push(format!(
                    ">{}:{}  {}  {}",
                    lens_letter,
                    row_mark,
                    format!("{:<96}", bytes.join(" ")),
                    ascii
                ));
                a += 32;
            }
            mon.state.mem_cursor = Some(((end as u32 + 1) & 0xffff) as u16);
            Ok(lines.join("\n"))
        }

        // ---- Disassembly: d [lens] [addr] [count|end] ------------------------
        "d" | "disass" => {
            let mut i = 1;
            let lens_tok = lens_of(toks.get(i));
            let lens = lens_tok.clone().unwrap_or_else(|| mon.state.bank_default.clone());
            if lens_tok.is_some() {
                i += 1;
            }
            let default_pc = if mon.state.device == "drive8" {
                host.machine().drive8.core.reg_pc
            } else {
                host.machine().cpu6510.reg_pc
            };
            let start = parse_addr(toks.get(i))
                .or(mon.state.disasm_cursor)
                .unwrap_or(default_pc);
            // `d <start> <end>` = RANGE (VICE). The 2nd arg, present, is an END addr.
            let end: Option<u16> = if toks.get(i + 1).is_some() {
                Some(parse_addr(toks.get(i + 1)).ok_or("d: bad end address")?)
            } else {
                None
            };
            if let Some(e) = end {
                if e < (start & 0xffff) {
                    return Err(format!("d: end ${:04x} < start ${:04x}", e, start & 0xffff));
                }
            }
            let pc = host.machine().cpu6510.reg_pc;
            // device drive8: disassemble the 1541 CPU address space (read-inspect).
            let on_drive = device == "drive8";
            // Spec 804 — no names here: the addresses are marked with their space (and
            // the lens they were read through), and C64RE names them. That is also what
            // closes the old leak of C64 labels into the drive's listing.
            let (space, span_lens) =
                if on_drive { (SpanSpace::Drive8, None) } else { (SpanSpace::C64, Some(lens.as_str())) };
            // Peek, deliberately, even under `sidefx on`: the renderer wants a plain
            // `Fn(u16) -> u8`, and a side-effecting read cannot be one — it needs &mut.
            // Reading a whole listing through live I/O would also mean a disassembly
            // silently changing the machine it is describing. `sidefx` covers the
            // inspection verbs (m/c/t/h) where the reference's own example lives.
            // `host.machine()` hands out `&mut Machine`, so a closure that calls it is
            // `FnMut`; the renderer wants `Fn`. Reborrow it shared once, here.
            let mach: &Machine = host.machine();
            let read = |x: u16| {
                if on_drive {
                    mach.drive8.drive_peek(x)
                } else {
                    mach.peek_lens(x, &lens)
                }
            };
            let mut lines: Vec<String> = Vec::new();
            let mut a = start & 0xffff;
            const MAX: usize = 4096;
            let mut n = 0usize;
            if let Some(e) = end {
                let e = e & 0xffff;
                while a <= e && n < MAX {
                    let (size, line) = addr_spans::disasm_line_in(read, a, space, span_lens);
                    lines.push(if a == pc { format!("{line} <-- PC") } else { line });
                    a = a.wrapping_add(size);
                    n += 1;
                    if a == 0 {
                        break; // wrapped past $FFFF
                    }
                }
                if a <= e && n >= MAX {
                    lines.push(format!(
                        "… (truncated at ${:04x} — `d ${:04x} ${:04x}` to continue)",
                        a, a, e
                    ));
                }
            } else {
                while n < 16 {
                    let (size, line) = addr_spans::disasm_line_in(read, a, space, span_lens);
                    lines.push(if a == pc { format!("{line} <-- PC") } else { line });
                    a = a.wrapping_add(size);
                    n += 1;
                }
            }
            mon.state.disasm_cursor = Some(a);
            Ok(lines.join("\n"))
        }

        // ---- Flow disassembly (Spec 754 §3.3k / audit ws-trace-monitor-misc-5) ----
        // sd [n] — DYNAMIC: step n instructions from PC, render the REAL executed
        // path (each touched address ONCE, loops folded to body + ×count), footer
        // `-- sd: N steps, K distinct addrs -> .C:<land>`. 1:1 with monitor-flow-
        // disasm.ts stepDisasm. Non-destructive: capture a machine checkpoint, step,
        // render, then restore (the live shared session must not advance). Reuses the
        // EXISTING step_one_instruction + disasm renderer (disasm_line_ts).
        "screen" => {
            let dd00 = host.machine().peek_lens(0xdd00, "io") & 0x03;
            let vic_bank = ((3 - dd00) as u16) * 0x4000; // CIA2 PA bits 0..1 inverted
            let d018 = host.machine().peek_lens(0xd018, "io");
            let screen_base = vic_bank.wrapping_add((((d018 >> 4) & 0x0f) as u16) * 0x0400);
            let mut lines: Vec<String> = vec![format!(
                "screen @ ${:04x}  (VIC bank ${:04x}, $D018=${:02x})",
                screen_base, vic_bank, d018
            )];
            for row in 0u16..25 {
                let mut line = String::new();
                for col in 0u16..40 {
                    let a = screen_base.wrapping_add(row * 40 + col);
                    line.push(sc_to_ascii(host.machine().peek_lens(a, "ram")));
                }
                lines.push(format!("|{line}|"));
            }
            Ok(lines.join("\n"))
        }

        // ---- bitmap <addr> [w] [h] [hires|charset|sprite] — render a RAM range
        // as an image (§3.3b, folds the Scrub tab). 1:1 with monitor-shell.ts:745-
        // 767: the text console can't inline it, so it writes a PNG artifact +
        // returns the path. w/h are DECIMAL counts (cells/rows/sprites per mode);
        // addr is hex. (multicolor = v1.1.) The help advertised it but run_monitor
        // had NO arm → `unknown command: bitmap` (the help LIED). charset/sprite
        // are MODES of this verb (matching TS), not standalone verbs.
        "f" | "fill" => {
            let start = parse_addr(toks.get(1)).ok_or("f: usage: f <start> <end> <byte..>")?;
            let end = parse_addr(toks.get(2)).ok_or("f: usage: f <start> <end> <byte..>")?;
            let data: Vec<Option<u8>> = toks[3..].iter().map(|t| parse_byte(Some(t))).collect();
            if data.is_empty() || data.iter().any(|b| b.is_none()) {
                return Err("f: need >=1 fill byte".into());
            }
            let data: Vec<u8> = data.into_iter().map(|b| b.unwrap()).collect();
            let mut n: usize = 0;
            let mut a = start as u32;
            while a <= end as u32 {
                let b = data[n % data.len()];
                // Banked, like the TS reference's `writeByte(..., "cpu")` — filling
                // $D020 must reach the VIC, not the RAM under the I/O window.
                monitor_write(host, (a & 0xffff) as u16, &[b], "cpu");
                n += 1;
                a += 1;
            }
            Ok(format!(
                "filled ${:04x}..${:04x} ({} bytes, pattern {})",
                start, end, n, data.len()
            ))
        }

        // ---- a <addr> [instr] — inline 6502 assembler (Spec 754 §3.3c). ------
        // `a c000 lda #$01` assembles that line then STAYS in modal assemble at the
        // next addr; `a c000` (no instr) ENTERS modal assemble at $C000 (the modal
        // interception at the top of run_monitor then takes every following line). The
        // help advertised it but run_monitor had NO arm → `unknown command: a` (the
        // help LIED). 1:1 with monitor-shell.ts:715-728 (op==="a").
        "a" => {
            let addr = parse_addr(toks.get(1)).ok_or(
                "a: usage: a <addr> [instruction]  — enter assemble mode (empty line exits)",
            )?;
            if toks.len() < 3 {
                // Enter modal assemble at addr; the interception handles subsequent lines.
                mon.state.asm_cursor = Some(addr);
                mon.state.disasm_cursor = Some(addr);
                mon.state.pending_prompt = Some(asm_prompt(addr));
                return Ok(String::new());
            }
            // Assemble the inline instruction (the rest of the line). This leaves the
            // session in modal assemble at the next addr (= TS, which stays in mode).
            let instr = toks[2..].join(" ");
            assemble_at(mon, host, addr, &instr)
        }

        // ---- t <start> <end> <dest> — move/copy (overlap-safe). --------------
        "t" | "move" => {
            let start = parse_addr(toks.get(1)).ok_or("t: usage: t <start> <end> <dest>")?;
            let end = parse_addr(toks.get(2)).ok_or("t: usage: t <start> <end> <dest>")?;
            let dest = parse_addr(toks.get(3)).ok_or("t: usage: t <start> <end> <dest>")?;
            // u32, NOT u16: a full-range move is 65536 bytes, and `65536 as u16` is 0 —
            // the loop then ran zero times and reported a successful move of nothing.
            let len = end as i32 - start as i32 + 1;
            if len <= 0 {
                return Err("t: end < start".into());
            }
            let len = len as u32;
            let mut buf: Vec<u8> = Vec::with_capacity(len as usize);
            for k in 0..len {
                buf.push(monitor_read(&mon.state, host.machine(), start.wrapping_add(k as u16), "cpu"));
            }
            for (k, b) in buf.iter().enumerate() {
                // Banked, matching the TS reference's hardcoded "cpu" lens for t/move.
                monitor_write(host, dest.wrapping_add(k as u16), &[*b], "cpu");
            }
            Ok(format!("moved {len} byte(s) ${start:04x}..${end:04x} -> ${dest:04x}"))
        }

        // ---- c <start> <end> <dest> — compare, list differences. -------------
        "c" | "compare" => {
            let start = parse_addr(toks.get(1)).ok_or("c: usage: c <start> <end> <dest>")?;
            let end = parse_addr(toks.get(2)).ok_or("c: usage: c <start> <end> <dest>")?;
            let dest = parse_addr(toks.get(3)).ok_or("c: usage: c <start> <end> <dest>")?;
            let len = end as i32 - start as i32 + 1;
            if len <= 0 {
                return Err("c: end < start".into());
            }
            // u32, NOT u16: `c $0000 $ffff <dest>` is 65536 bytes, and `65536 as u16` is
            // 0 — the loop ran zero times, `diffs` stayed empty, and the answer was a
            // confident "identical" for two ranges that had never been looked at.
            let len = len as u32;
            let mut diffs: Vec<String> = Vec::new();
            for k in 0..len {
                let k = k as u16;
                let av = monitor_read(&mon.state, host.machine(), start.wrapping_add(k), "cpu");
                let bv = monitor_read(&mon.state, host.machine(), dest.wrapping_add(k), "cpu");
                if av != bv {
                    diffs.push(format!(
                        "  ${:04x}: {:02x} != {:02x} @${:04x}",
                        start.wrapping_add(k), av, bv, dest.wrapping_add(k)
                    ));
                }
                if diffs.len() > 64 {
                    diffs.push("  ... (truncated)".to_string());
                    break;
                }
            }
            Ok(if diffs.is_empty() {
                format!("identical (${start:04x}..${end:04x} == ${dest:04x})")
            } else {
                format!("differences:\n{}", diffs.join("\n"))
            })
        }

        // ---- h <start> <end> <byte/xx..> — hunt/search (xx or * = wildcard). --
        "h" | "hunt" => {
            let start = parse_addr(toks.get(1)).ok_or("h: usage: h <start> <end> <byte/xx..>")?;
            let end = parse_addr(toks.get(2)).ok_or("h: usage: h <start> <end> <byte/xx..>")?;
            let mut pat: Vec<i32> = Vec::new();
            let mut bad = toks.len() < 4;
            for t in &toks[3..] {
                if t.eq_ignore_ascii_case("xx") || t == "*" {
                    pat.push(-1);
                } else if let Some(b) = parse_byte(Some(t)) {
                    pat.push(b as i32);
                } else {
                    bad = true;
                }
            }
            if pat.is_empty() || bad {
                return Err("h: need >=1 pattern byte (xx = wildcard)".into());
            }
            let mut hits: Vec<u16> = Vec::new();
            let mut a = start as i32;
            while a + (pat.len() as i32) - 1 <= end as i32 {
                let mut m = true;
                for (k, pb) in pat.iter().enumerate() {
                    if *pb != -1
                        && monitor_read(&mon.state, host.machine(), (a as u16).wrapping_add(k as u16), "cpu") as i32 != *pb
                    {
                        m = false;
                        break;
                    }
                }
                if m {
                    hits.push(a as u16);
                    if hits.len() > 256 {
                        break;
                    }
                }
                a += 1;
            }
            Ok(if hits.is_empty() {
                "not found".to_string()
            } else {
                format!(
                    "found {}:\n  {}",
                    hits.len(),
                    hits.iter().map(|a| format!("${:04x}", a)).collect::<Vec<_>>().join(" ")
                )
            })
        }

        // ---- Bank lens default (§3.3b/§3.3d): bank [cpu|ram|rom|io|cart]. ----
        "bank" => {
            let arg = toks.get(1).map(|s| s.to_ascii_lowercase()).unwrap_or_default();
            if arg.is_empty() {
                return Ok(format!(
                    "bank = {}  (lens for m/d; one of cpu|ram|rom|io|cart)",
                    mon.state.bank_default
                ));
            }
            if LENSES.contains(&arg.as_str()) {
                mon.state.bank_default = arg.clone();
                Ok(format!("bank = {arg}"))
            } else {
                Err(format!("bank: expected cpu|ram|rom|io|cart, got '{arg}'"))
            }
        }

        // ---- sidefx [on|off|toggle] (§3.4). ----------------------------------
        "sidefx" => {
            let arg = toks.get(1).map(|s| s.to_ascii_lowercase()).unwrap_or_else(|| "toggle".into());
            let cur = mon.state.sidefx_on;
            let next = match arg.as_str() {
                "on" => Some(true),
                "off" => Some(false),
                "toggle" => Some(!cur),
                _ => None,
            };
            let next = next.ok_or("sidefx: on|off|toggle")?;
            mon.state.sidefx_on = next;
            Ok(if next {
                "sidefx = on (m/c/t/h read LIVE — I/O side effects; d/sd/bitmap stay peeks)"
                    .to_string()
            } else {
                "sidefx = off (peek — side-effect-free, default)".to_string()
            })
        }

        // ---- Breakpoints: bk | bk <addr> | bk -<addr> | bk clear ------------
        // ---- Observers (Spec 754 §3.3e) — the full DSL the c64re REPL exposes. ----
        //   obs <name> when exec|load|store <addr[..end]> [if <cond>] do break|log|mark|cmd|trace
        //   obs | o                  list registered observers
        //   obs log                  recent `do log` lines
        //   obs <name> on|off        enable/disable
        //   obs <name> del|rm        remove
        //   ignore <name> [n]        skip the next n triggers
        // 1:1 with monitor-shell.ts:888-1001 (which dispatches `obs`/`o`/`ignore` —
        // there is NO `reg` verb, so TRX64 must not add one or it would diverge). The
        // parsed spec is stored in `mon.dsl_observers` (survives the per-run
        // sync_observers rebuild) and re-applied onto the live registry every run;
        // `o` / bare `obs` list that store.
        "obs" | "o" | "ignore" => {
            // Render one stored observer the way the c64re `fmt` closure does
            // (monitor-shell.ts:898): `  * name  trigger $lo[..hi] [if cond] do <do>  hits=N`.
            let fmt_obs = |spec: &observers::ObsSpec,
                           reg: &observers::ObserverRegistry,
                           disabled: &std::collections::HashSet<String>|
             -> String {
                let live = reg.get(&spec.name);
                // A disabled DSL observer is absent from the live registry (not re-armed),
                // so derive `enabled` from the persisted disable-set, not the registry.
                let enabled = !disabled.contains(&spec.name);
                let hits = live.map(|o| o.hits).unwrap_or(0);
                let trig = match spec.trigger {
                    observers::ObsTrigger::Exec => "exec",
                    observers::ObsTrigger::Load => "load",
                    observers::ObsTrigger::Store => "store",
                };
                let range = if spec.hi != spec.lo {
                    format!("${:04x}..${:04x}", spec.lo, spec.hi)
                } else {
                    format!("${:04x}", spec.lo)
                };
                let cond = spec
                    .cond_src
                    .as_ref()
                    .map(|c| format!(" if {c}"))
                    .unwrap_or_default();
                let do_desc = obs_do_desc(spec);
                format!(
                    "  {} {}  {} {}{} do {}  hits={}",
                    if enabled { "*" } else { "o" },
                    spec.name,
                    trig,
                    range,
                    cond,
                    do_desc,
                    hits
                )
            };

            // `ignore <name> [n]` — set the per-observer ignore count.
            if op == "ignore" {
                let name = match toks.get(1) {
                    Some(n) => n.clone(),
                    None => return Err("ignore: usage: ignore <name> [n]".into()),
                };
                let n: i64 = toks.get(2).and_then(|t| t.parse().ok()).unwrap_or(1);
                let found = mon.dsl_observers.iter().any(|o| o.name == name);
                if !found {
                    return Ok(format!("no observer '{name}'"));
                }
                // Mirror onto the live registry so the next run honours it; the count is
                // preserved across rebuilds via sync_observers' `prior` snapshot.
                //
                // A DISABLED observer is not in that registry at all (sync_observers skips
                // it), so there is nothing to arm — `set_ignore` returns false and the
                // count used to vanish while the reply still said "skip next N". Report
                // the real state instead of a number that will never take effect.
                if !mon.observers.set_ignore(&name, n) {
                    return Ok(format!(
                        "ignore {name}: observer is off — `obs {name} on` first, then set the count"
                    ));
                }
                return Ok(format!("ignore {name}: skip next {n}"));
            }

            let rest: Vec<String> = toks[1..].to_vec();

            // No args (or bare `reg`/`o`) → LIST.
            if rest.is_empty() {
                // sync so the live registry reflects current enabled/hits state.
                {
                    let MonitorSession { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *mon;
                    sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
                }
                if mon.dsl_observers.is_empty() {
                    return Ok("no observers (obs <name> when exec|load|store <addr> [if <cond>] do break|log|mark|cmd|trace)".into());
                }
                let lines: Vec<String> = mon
                    .dsl_observers
                    .iter()
                    .map(|s| fmt_obs(s, &mon.observers, &mon.dsl_disabled))
                    .collect();
                return Ok(format!("observers:\n{}", lines.join("\n")));
            }

            // `obs log` → recent `do log` ring.
            if rest[0].eq_ignore_ascii_case("log") {
                let logs = &mon.observers.logs;
                if logs.is_empty() {
                    return Ok("obs log: (empty)".into());
                }
                let start = logs.len().saturating_sub(40);
                return Ok(logs[start..].join("\n"));
            }

            let name = rest[0].clone();
            let sub = rest.get(1).map(|s| s.to_ascii_lowercase()).unwrap_or_default();

            // A name containing `*`/`?` is a GLOB → on/off/del act on ALL matches
            // (`obs * del` = all, `obs c* off` = every observer starting "c"). 1:1 with
            // monitor-shell.ts:909-932 (audit monitor-obs-lifecycle). TRX64 previously
            // matched the name EXACTLY, so a glob matched no literal observer → "no
            // observer '*'" while the help advertised the wildcard (the help LIED).
            let is_glob = name.contains('*') || name.contains('?');
            // Expand the glob to the matching observer names. `*` = any run (incl.
            // empty), `?` = exactly one char — anchored full-string match, 1:1 with the
            // TS globMatches() regex (`^` + `*`→".*" + `?`→"." + `$`). Observer names
            // are plain identifiers (no regex metachars), so a direct glob walk suffices.
            let glob_matches = |mon: &MonitorSession| -> Vec<String> {
                mon.dsl_observers
                    .iter()
                    .map(|o| o.name.clone())
                    .filter(|n| glob_full_match(&name, n))
                    .collect()
            };

            // `obs <name> on|off` — persist the disable intent in `dsl_disabled` so it
            // survives the per-run sync_observers rebuild; re-sync to apply immediately.
            if rest.len() == 2 && (sub == "on" || sub == "off") {
                if is_glob {
                    let matches = glob_matches(mon);
                    if matches.is_empty() {
                        return Ok(format!("no observer matches '{name}'"));
                    }
                    for m in &matches {
                        if sub == "off" { mon.dsl_disabled.insert(m.clone()); }
                        else { mon.dsl_disabled.remove(m); }
                    }
                    {
                        let MonitorSession { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *mon;
                        sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
                    }
                    return Ok(format!("{sub} {}: {}", matches.len(), matches.join(", ")));
                }
                if !mon.dsl_observers.iter().any(|o| o.name == name) {
                    return Ok(format!("no observer '{name}'"));
                }
                if sub == "off" {
                    mon.dsl_disabled.insert(name.clone());
                } else {
                    mon.dsl_disabled.remove(&name);
                }
                {
                    let MonitorSession { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *mon;
                    sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
                }
                return Ok(format!("obs {name} {sub}"));
            }

            // `obs <name> del|delete|rm`
            if rest.len() == 2 && (sub == "del" || sub == "delete" || sub == "rm") {
                if is_glob {
                    let matches = glob_matches(mon);
                    if matches.is_empty() {
                        return Ok(format!("no observer matches '{name}'"));
                    }
                    for m in &matches {
                        mon.dsl_observers.retain(|o| &o.name != m);
                        mon.observers.remove(m);
                        mon.dsl_disabled.remove(m);
                    }
                    return Ok(format!("deleted {}: {}", matches.len(), matches.join(", ")));
                }
                let before = mon.dsl_observers.len();
                mon.dsl_observers.retain(|o| o.name != name);
                if mon.dsl_observers.len() != before {
                    mon.observers.remove(&name);
                    mon.dsl_disabled.remove(&name);
                    return Ok(format!("obs {name} deleted"));
                }
                return Ok(format!("no observer '{name}'"));
            }

            // `obs <name> when exec|load|store <addr[..end]> [if <cond>] do <action> [fields]`
            let lower: Vec<String> = rest.iter().map(|t| t.to_ascii_lowercase()).collect();
            let wi = lower.iter().position(|t| t == "when");
            let di = lower.iter().rposition(|t| t == "do");
            let ii = lower.iter().position(|t| t == "if");
            // `when` must be the token right after the name (index 1), and `do` after it.
            let (wi, di) = match (wi, di) {
                (Some(wi), Some(di)) if wi == 1 && di > wi => (wi, di),
                _ => {
                    return Err(
                        "obs: usage: obs <name> when exec|load|store <addr[..end]> [if <cond>] do break|log|mark|cmd|trace [a/x/y/$addr ...]"
                            .into(),
                    )
                }
            };
            let trig_s = lower[wi + 1].clone();
            let trigger = match trig_s.as_str() {
                "exec" => observers::ObsTrigger::Exec,
                "load" => observers::ObsTrigger::Load,
                "store" => observers::ObsTrigger::Store,
                _ => {
                    return Err(format!(
                        "obs: trigger must be exec|load|store, got '{}'",
                        rest.get(wi + 1).cloned().unwrap_or_default()
                    ))
                }
            };
            let addr_tok = rest.get(wi + 2).cloned().unwrap_or_default();
            let (lo_s, hi_s) = match addr_tok.split_once("..") {
                Some((a, b)) => (a.to_string(), Some(b.to_string())),
                None => (addr_tok.clone(), None),
            };
            let lo = match parse_hex(&lo_s) {
                Some(v) => (v & 0xffff) as u16,
                None => return Err(format!("obs: bad address '{addr_tok}'")),
            };
            let hi = match &hi_s {
                Some(h) => match parse_hex(h) {
                    Some(v) => (v & 0xffff) as u16,
                    None => return Err(format!("obs: bad address '{addr_tok}'")),
                },
                None => lo,
            };
            let action_s = lower.get(di + 1).cloned().unwrap_or_default();
            let action = match action_s.as_str() {
                "break" => observers::ObsAction::Break,
                "log" => observers::ObsAction::Log,
                "mark" => observers::ObsAction::Mark,
                "cmd" => observers::ObsAction::Cmd,
                "trace" => observers::ObsAction::Trace,
                _ => {
                    return Err(format!(
                        "obs: action must be break|log|mark|cmd|trace, got '{}'",
                        if action_s.is_empty() { "(none)" } else { &action_s }
                    ))
                }
            };
            // `*`/`?` reserved for the on/off/del wildcards (monitor-shell.ts:951).
            if name.contains('*') || name.contains('?') {
                return Err(format!(
                    "obs: name can't contain * or ? (reserved for wildcards) — got '{name}'"
                ));
            }
            // cond is the tokens between `if` and `do`.
            let cond_src = match ii {
                Some(ii) if ii > wi && ii < di => Some(rest[ii + 1..di].join(" ")),
                _ => None,
            };
            // do-action payloads (the tokens after `do <action>`).
            let do_toks: Vec<String> = rest[(di + 2).min(rest.len())..].to_vec();
            let mut log_exprs: Option<Vec<observers::LogExpr>> = None;
            let mut cmd_src: Option<String> = None;
            let mut mark_label: Option<String> = None;
            let mut trace_scope: Option<observers::TraceScope> = None;
            match action {
                observers::ObsAction::Log if !do_toks.is_empty() => {
                    let mut exprs: Vec<observers::LogExpr> = Vec::new();
                    for t in &do_toks {
                        let lw = t.to_ascii_lowercase();
                        let reg = match lw.as_str() {
                            "a" => Some(observers::RegName::A),
                            "x" => Some(observers::RegName::X),
                            "y" => Some(observers::RegName::Y),
                            "sp" => Some(observers::RegName::Sp),
                            "pc" => Some(observers::RegName::Pc),
                            "fl" => Some(observers::RegName::Fl),
                            _ => None,
                        };
                        if let Some(r) = reg {
                            exprs.push(observers::LogExpr::Reg(r));
                            continue;
                        }
                        let word = lw.ends_with(":w");
                        let addr_part = if word { &t[..t.len() - 2] } else { t.as_str() };
                        match parse_hex(addr_part) {
                            Some(a) => exprs.push(observers::LogExpr::Mem {
                                addr: (a & 0xffff) as u16,
                                word,
                            }),
                            None => {
                                return Err(format!(
                                    "obs: log: bad field '{t}' (use a/x/y/sp/pc/fl or $addr[:w])"
                                ))
                            }
                        }
                    }
                    log_exprs = Some(exprs);
                }
                observers::ObsAction::Cmd => {
                    // do cmd "<monitor command>" — quoted command run on each hit.
                    match quoted_first(&cmd) {
                        Some(c) if !c.is_empty() => cmd_src = Some(c),
                        _ => return Err(r#"obs: cmd: usage: ... do cmd "<monitor command>""#.into()),
                    }
                }
                observers::ObsAction::Mark => {
                    // do mark ["label"] — default label = the observer name.
                    mark_label = Some(quoted_first(&cmd).unwrap_or_else(|| name.clone()));
                }
                observers::ObsAction::Trace => {
                    // do trace off | do trace [domains...] — bracket model.
                    let args: Vec<String> = do_toks.iter().map(|t| t.to_ascii_lowercase()).collect();
                    if args.first().map(|s| s == "off").unwrap_or(false) {
                        trace_scope = Some(observers::TraceScope { off: true, domains: vec![] });
                    } else {
                        const ALL: [&str; 7] =
                            ["c64-cpu", "drive8-cpu", "iec", "vic", "memory", "drive-mechanism", "cart-read"];
                        if let Some(bad) = args.iter().find(|d| !ALL.contains(&d.as_str())) {
                            return Err(format!(
                                "obs: trace: unknown domain '{bad}' (use {} or 'off')",
                                ALL.join("|")
                            ));
                        }
                        let domains = if args.is_empty() {
                            vec!["c64-cpu".to_string(), "memory".to_string()]
                        } else {
                            args
                        };
                        trace_scope = Some(observers::TraceScope { off: false, domains });
                    }
                }
                observers::ObsAction::Break if !do_toks.is_empty() => {
                    return Err(format!(
                        "obs: 'break' takes no fields (got '{}')",
                        do_toks.join(" ")
                    ));
                }
                _ => {}
            }

            // Validate the condition NOW (so a bad cond errors at registration, like TS).
            if let Some(src) = &cond_src {
                if let Err(e) = observers::parse_cond(src) {
                    return Err(format!("obs: condition: {e}"));
                }
            }

            let spec = observers::ObsSpec {
                name: name.clone(),
                trigger,
                lo,
                hi,
                cond_src: cond_src.clone(),
                action,
                log_exprs: log_exprs.clone(),
                cmd_src: cmd_src.clone(),
                mark_label: mark_label.clone(),
                trace_scope: trace_scope.clone(),
            };
            // Replace an existing same-name registration; else append.
            if let Some(slot) = mon.dsl_observers.iter_mut().find(|o| o.name == name) {
                *slot = spec;
            } else {
                mon.dsl_observers.push(spec);
            }
            // Apply onto the live registry immediately so a running --stream loop arms it
            // on the next frame (sync_observers re-applies it thereafter).
            {
                let MonitorSession { breakpoints, dsl_observers, dsl_disabled, observers: reg, .. } = &mut *mon;
                sync_observers(breakpoints, dsl_observers, dsl_disabled, reg);
            }

            let trig_str = match trigger {
                observers::ObsTrigger::Exec => "exec",
                observers::ObsTrigger::Load => "load",
                observers::ObsTrigger::Store => "store",
            };
            let range = if hi != lo {
                format!("${lo:04x}..${hi:04x}")
            } else {
                format!("${lo:04x}")
            };
            let cond_disp = cond_src.map(|c| format!(" if {c}")).unwrap_or_default();
            let do_disp = obs_do_desc(mon.dsl_observers.last().unwrap());
            return Ok(format!("obs {name}: {trig_str} {range}{cond_disp} do {do_disp}"));
        }

        "bk" | "break" | "b" => {
            let t1 = toks.get(1);
            match t1 {
                None => {
                    let list = &mon.breakpoints.entries;
                    Ok(if list.is_empty() {
                        "no breakpoints (set: bk <addr>)".to_string()
                    } else {
                        let mut s = String::from("breakpoints:");
                        for e in list {
                            s.push_str(&format!(
                                "\n  #{}  {}",
                                e.num,
                                addr_spans::addr4(e.pc, SpanSpace::C64, SpanRole::Pc)
                            ));
                        }
                        s
                    })
                }
                Some(t1) if t1.eq_ignore_ascii_case("clear") => {
                    mon.breakpoints.entries.clear();
                    Ok("breakpoints cleared".to_string())
                }
                Some(t1) if t1.starts_with('-') => {
                    let a = parse_addr(Some(&t1[1..].to_string()))
                        .ok_or_else(|| format!("bad address: {t1}"))?;
                    mon.breakpoints.entries.retain(|e| e.pc != a);
                    Ok(format!("removed bp ${:04x} ({} left)", a, mon.breakpoints.entries.len()))
                }
                Some(t1) => {
                    let addr = parse_addr(Some(t1)).ok_or_else(|| format!("bad address: {t1}"))?;
                    let num = mon.breakpoints.next_num;
                    mon.breakpoints.next_num += 1;
                    mon.breakpoints.entries.push(BpEntry { num, pc: addr, enabled: true });
                    Ok(format!(
                        "bk #{} set at {} ({} total)",
                        num,
                        addr_spans::addr4(addr, SpanSpace::C64, SpanRole::Pc),
                        mon.breakpoints.entries.len()
                    ))
                }
            }
        }

        // ---- Delete breakpoint(s): del | del <num> ... ----------------------
        "del" | "delete" => {
            if toks.get(1).is_none() {
                mon.breakpoints.entries.clear();
                return Ok("all breakpoints deleted".to_string());
            }
            let mut out: Vec<String> = Vec::new();
            for t in &toks[1..] {
                match t.parse::<u32>() {
                    Err(_) => out.push(format!("bad checknum: {t}")),
                    Ok(num) => {
                        let before = mon.breakpoints.entries.len();
                        mon.breakpoints.entries.retain(|e| e.num != num);
                        if mon.breakpoints.entries.len() < before {
                            out.push(format!("deleted #{num}"));
                        } else {
                            out.push(format!("no breakpoint #{num}"));
                        }
                    }
                }
            }
            Ok(out.join("\n"))
        }

        // ---- Go / resume (§3.1). g [addr] / x ; enters the run-loop. ---------
        // TRX64 daemon is request/response with no autonomous loop. `g` mirrors
        // the TS BUG-036 contract shape: set PC (if given), step past a parked
        // breakpoint, mark running, and report ".C:PC (running — Pause to halt)".
        // The actual advance happens on the next debug/run (the run-loop), exactly
        // like TS where `ctrl.continue()` flips run-state and the tick loop runs.
        "flow" => {
            // FlowTracker.render() (stepping.ts:174-190 + monitor-shell.ts:1103-1117):
            //   `flow: current=<kind>  focus=<focus>\nframes:\n<lines | placeholder>`.
            // At the cold/rest state the stack is empty → current=main; after stepping
            // into an interrupt it is state-dependent (current=irq|nmi|brk + frames).
            Ok(mon.flow.render())
        }

        // ---- tracedb — run a STORED trace definition (monitor-shell.ts:445-470) ------
        //
        // `trace on` captures everything; `tracedb start "<id>"` runs a definition that
        // was put into the registry (trace/definition/put) and records which one, so the
        // resulting store says what it was capturing and why. The registry was ported;
        // only the verb in front of it was missing.
        //
        // start/stop/status DELEGATE to the `trace` verb rather than repeating its ~60
        // lines of store setup — two copies of that would drift, and drift is what this
        // whole exercise is about.
        "io" => {
            let arg = toks.get(1).map(|s| s.as_str());
            let want_details = arg.is_some();
            let filter: Option<u16> = match arg {
                None | Some("1") => None,
                Some(a) => Some(
                    parse_addr(Some(&a.to_string())).ok_or("io: usage: io [1 | <addr>]")?,
                ),
            };
            let mut blocks: Vec<(String, u16, u16)> = vec![
                ("VIC-II".into(), 0xd000, 0xd03f),
                ("SID".into(), 0xd400, 0xd41f),
                ("CIA1".into(), 0xdc00, 0xdc0f),
                ("CIA2".into(), 0xdd00, 0xdd0f),
            ];
            if let Some(c) = host.machine().cartridge.as_ref() {
                let nm = mapper_type_str(c.mapper_type()).to_ascii_uppercase();
                blocks.push((format!("{nm} (IO1)"), 0xde00, 0xde0f));
                blocks.push((format!("{nm} (IO2)"), 0xdf00, 0xdf0f));
            }
            let mut lines: Vec<String> = Vec::new();
            for (name, start, end) in &blocks {
                let page = start & 0xff00;
                if let Some(f) = filter {
                    if f < page || f > (page | 0xff) {
                        continue;
                    }
                }
                lines.push(format!("{name}:"));
                let mut row = *start;
                while row <= *end {
                    let mut bytes: Vec<String> = Vec::new();
                    let mut i = 0u16;
                    while i < 16 && row.wrapping_add(i) <= *end {
                        bytes.push(format!(
                            "{:02x}",
                            host.machine().peek_lens(row.wrapping_add(i), "io")
                        ));
                        i += 1;
                    }
                    lines.push(format!("  {row:04x}  {}", bytes.join(" ")));
                    row = row.wrapping_add(16);
                }
                if want_details {
                    lines.push("  No details available.".to_string());
                }
                lines.push(String::new());
            }
            if lines.is_empty() {
                return Err(format!("io: no device at ${:04x}", filter.unwrap_or(0)));
            }
            Ok(lines.join("\n").trim_end().to_string())
        }

        // ---- IEC BUS ---------------------------------------------------------------
        //
        // Three wires and a wired-AND, and until now nothing showed them. A serial
        // stall is always the same question — WHICH line is low and WHO is holding it
        // — and it was unanswerable from outside: `m dd00` returns the CIA latch, the
        // drive's $1800 peeked as zero, and the bus state lives in the IEC core where
        // no verb reached. Diagnosing a hang meant single-stepping the KERNAL until an
        // `LDA $DD00` landed in the accumulator, one byte at a time.
        //
        // A released line reads 1 and a device pulls it to 0, so "who is low" is a
        // per-device answer, not a bus-wide one. That is why the table has a column
        // per driver rather than a single state.
        "iec" => {
            let m = &host.machine();
            let cpu_bus = m.iec.cpu_bus();
            let cpu_port = m.iec.cpu_port();
            let drv_port = m.iec.drv_port();
            let drv_bus8 = m.iec.drv_bus(8);
            let drv_data8 = m.iec.drv_data(8);

            // cpu_bus / cpu_port bit positions (c64iec.c iec_update_cpu_bus).
            const DATA: u8 = 0x80;
            const CLK: u8 = 0x40;
            const ATN: u8 = 0x10;
            let lvl = |v: u8, bit: u8| if v & bit != 0 { "high" } else { "LOW " };
            let pull = |v: u8, bit: u8| if v & bit != 0 { "-" } else { "pulls" };

            let mut out = vec![
                "IEC BUS  (a released line is high; any device may pull it low)".to_string(),
                "  line   bus     C64     drive 8".to_string(),
                format!(
                    "  ATN    {}    {}   {}",
                    lvl(cpu_bus, ATN),
                    pull(cpu_bus, ATN),
                    "-      (ATN is C64-only)"
                ),
                format!(
                    "  CLK    {}    {}   {}",
                    lvl(cpu_port, CLK),
                    pull(cpu_bus, CLK),
                    pull(drv_bus8, CLK)
                ),
                format!(
                    "  DATA   {}    {}   {}",
                    lvl(cpu_port, DATA),
                    pull(cpu_bus, DATA),
                    pull(drv_bus8, DATA)
                ),
                String::new(),
                format!(
                    "  cpu_bus=${cpu_bus:02x}  cpu_port=${cpu_port:02x}  \
                     drv_bus[8]=${drv_bus8:02x}  drv_port=${drv_port:02x}  \
                     drv_data[8]=${drv_data8:02x}"
                ),
            ];

            // Both ends, as the CPUs actually see them — not as the latches read.
            let dd00 = m.read_full(0xdd00);
            let dd02 = m.peek_lens(0xdd02, "io");
            out.push(format!(
                "  C64  $DD00 = ${dd00:02x} (DDR ${dd02:02x})   bit7 DATA in={} bit6 CLK in={} \
                 bit3 ATN out={}",
                (dd00 >> 7) & 1,
                (dd00 >> 6) & 1,
                (dd02 & 0x08 != 0) as u8 * ((m.peek_lens(0xdd00, "io") >> 3) & 1)
            ));
            let v1800 = host.machine().drive8.drive_peek(0x1800);
            let d1802 = host.machine().drive8.drive_peek(0x1802);
            out.push(format!(
                "  1541 $1800 = ${v1800:02x} (DDR ${d1802:02x})   bit7 ATN in={} bit4 ATNA={} \
                 bit3 CLK out={} bit2 CLK in={} bit1 DATA out={} bit0 DATA in={}",
                (v1800 >> 7) & 1,
                (v1800 >> 4) & 1,
                (v1800 >> 3) & 1,
                (v1800 >> 2) & 1,
                (v1800 >> 1) & 1,
                v1800 & 1
            ));
            Ok(out.join("\n"))
        }

        // ---- Flow FOCUS (monitor-shell.ts:1075-1099) --------------------------------
        //
        // The port took `flow` — the panel that DISPLAYS the focus — and left behind the
        // four verbs that set and use it. So the panel showed `focus=auto` forever, with
        // no way to change it, and the help plus the cockpit's completion kept offering
        // verbs that answered "unknown verb". The whole point is stepping inside one
        // flow: `focus irq` then `sf` walks the raster interrupt without descending into
        // whatever main-line code it interrupted.
        "focus" => {
            let arg = toks.get(1).map(|s| s.to_ascii_lowercase()).unwrap_or_default();
            if arg.is_empty() {
                let stack = if mon.flow.stack.is_empty() {
                    "  (main — no interrupt/trap frame active)".to_string()
                } else {
                    mon.flow
                        .stack
                        .iter()
                        .map(|f| {
                            // This port's frame records the RETURN pc where the reference
                            // records the entry SP; report what we actually have.
                            format!(
                                "  {}  enter={} ret={}",
                                f.kind.tag(),
                                addr_spans::addr4(f.entered_at_pc, SpanSpace::C64, SpanRole::Pc),
                                addr_spans::addr4(f.return_pc, SpanSpace::C64, SpanRole::Pc)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                return Ok(format!(
                    "focus = {} (current flow: {})\nflow stack:\n{stack}",
                    mon.flow.focus,
                    mon.flow.current_flow().tag()
                ));
            }
            if matches!(arg.as_str(), "auto" | "main" | "irq" | "nmi" | "brk" | "none" | "clear") {
                mon.flow.focus = if arg == "clear" { "none".to_string() } else { arg };
                Ok(format!("focus = {}", mon.flow.focus))
            } else {
                Err(format!("focus: expected auto|main|irq|nmi|brk|clear, got '{arg}'"))
            }
        }

        // `sf`/`stepf` — step into, but keep stepping until we are back in the target
        // flow. `nf`/`nextf` — the same, stepping OVER calls on the way.
        "bt" => {
            // buildBacktrace (backtrace.ts): scan $0100+((sp+1)&0xff) .. $01FF in
            // 2-byte steps for JSR return-address candidates (ret = (hi<<8|lo)+1),
            // up to 16. Reads via the cpu lens (peek, no side effect). State-dependent
            // on the live SP + stack bytes — NOT a constant.
            let m = &host.machine();
            let sp = (m.cpu6510.reg_sp & 0xff) as u32;
            let mut lines: Vec<String> =
                vec!["backtrace (live stack scan — best-effort; refine with `chis`):".to_string()];
            let mut found = 0usize;
            let mut a: u32 = 0x0100 + ((sp + 1) & 0xff);
            while a <= 0x01ff && found < 16 {
                let lo = m.peek_lens((a & 0xffff) as u16, "cpu") as u32;
                let hi = m.peek_lens(((a + 1) & 0xffff) as u16, "cpu") as u32;
                let ret = (((hi << 8) | lo) + 1) & 0xffff;
                // Spec 804 — the stack slot is memory, the return address is code.
                lines.push(format!(
                    "  {}: -> {}  (JSR return?)",
                    addr_spans::addr4((a & 0xffff) as u16, SpanSpace::C64, SpanRole::Memory),
                    addr_spans::addr4(ret as u16, SpanSpace::C64, SpanRole::Pc)
                ));
                found += 1;
                a += 2;
            }
            if found == 0 {
                lines.push("  (stack empty — SP at top)".to_string());
            }
            // backtrace.ts:35-38 — append the EXACT FlowTracker IRQ/NMI/BRK frames
            // (more than VICE) when the flow stack is non-empty.
            if !mon.flow.stack.is_empty() {
                lines.push("flow frames (exact, from stepping):".to_string());
                for fr in &mon.flow.stack {
                    lines.push(format!(
                        "  {} @ {}",
                        fr.kind.tag(),
                        addr_spans::addr4(fr.entered_at_pc, SpanSpace::C64, SpanRole::Pc)
                    ));
                }
            }
            Ok(lines.join("\n"))
        }

        // reverse-debug Phase 1b — `rstep`/`reverse [n]`: UNDO the last n instructions
        // from the always-on full-delta ring (default 1). Restores CPU + RAM +
        // IO-register BYTES, NOT chip internal counters → INSPECT-backward only (to
        // resume forward, restore a checkpoint anchor). Reports the landed PC/regs +
        // the writes rolled back.
        "triage" => {
            let at_pc = parse_addr(toks.get(1));
            let chain = host.machine().crash_triage(at_pc);
            // TRX64 feature-request #3 — the typed ring-exhaustion signal. The triage
            // bottomed out at the ring boundary when the wild transfer is older than the
            // ring (`ring_bound`); confirm + size it with the machine's ring state.
            let exhaustion = host.machine().ring_exhaustion(!chain.transfer.ring_bound);
            let mut lines = format_triage_lines(&chain);
            if chain.transfer.ring_bound && exhaustion.ring_exhausted {
                lines.push(format!(
                    "ring_exhausted: true   revdepth={}s   hint: {}",
                    exhaustion.revdepth_seconds, exhaustion.hint
                ));
            }
            Ok(lines.join("\n"))
        }

        // TRX64 feature-request #4 — `traprules <path>` loads project-supplied on-trap
        // dump rules from a JSON file; `traprules` (no arg) lists the loaded rules;
        // `traprules clear` drops them. On reaching/halting at a rule's PC (JAM /
        // breakpoint), the debugger auto-emits `label: name=$XX (decode)` reading the
        // project-named diagnostic bytes — NO built-in engine knowledge in the core.
        // ---- Help ------------------------------------------------------------
        "help" | "?" => Ok(monitor_help_text()),
        _ => Err(format!("unknown command: {op}. Try 'help'.")),
    }
}
