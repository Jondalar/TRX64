//! The ratatui cockpit — live panels + a command line, on a worker thread.
//!
//! Layout (top→bottom):
//!   ┌ CPU ───────────┐┌ MACHINE ───────┐┌ VIC ───────────┐
//!   │ PC A X Y SP P  ││ run/pause warp ││ raster mode bg ││   (3 side-by-side gauges)
//!   └────────────────┘└────────────────┘└────────────────┘
//!   ┌ FLOW / VECTORS ────────────────────────────────────┐  (one line)
//!   ┌ OUTPUT / LOG ──────────────────────────────────────┐  (scrolling, fills)
//!   ┌ command: > _ ──────────────────────────────────────┐  (input line)
//!
//! The cockpit drives the SAME `Engine` the pump + window share. The `window` verb
//! is delivered to the main thread over an mpsc channel (winit's EventLoop must own
//! the main thread on macOS); everything else is handled inline.

use std::cell::Cell;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode as XKeyCode, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::{json, Value};

use crate::engine::{Engine, StateSnapshot};

/// A message the cockpit sends to the main thread.
pub enum UiToMain {
    /// The `window` verb was issued — create/show the emulator window.
    OpenWindow,
    /// The cockpit is exiting (`quit`) — the main thread should tear down.
    Quit,
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Max command-history entries kept in memory + on disk (bash-ish). Applies to the
/// in-memory ring ([`Cockpit::push_history`]), the loaded on-disk tail
/// ([`load_history_from`]), and the persisted file itself ([`append_history_line`], which
/// compacts back down to this cap).
const HIST_CAP: usize = 2000;

/// How far the on-disk history file may overshoot [`HIST_CAP`] before
/// [`append_history_line`] rewrites it down to the last [`HIST_CAP`] entries. The slack
/// amortises the rewrite so a hot session isn't rewriting the whole file on every command
/// once it fills up (bash `HISTFILESIZE`, with headroom).
const HIST_FILE_SLACK: usize = 256;

/// One line in the OUTPUT/LOG pane. `style`, when set, overrides the draw-time content
/// heuristic (banner / `> ` echo) — used by `!ls` filetype coloring (S4) and, later,
/// the colored Tab candidate lists (S5). Plain lines carry `style: None` and keep the
/// existing content-based styling in [`draw_log`].
#[derive(Debug, Clone, PartialEq)]
struct LogLine {
    text: String,
    style: Option<Style>,
    /// When set, the line renders from these per-span (text, style) pairs instead of the
    /// single `text`+`style` pair — used by the structured help formatter (S10) to color
    /// the verb column green while the description stays default-fg on the SAME physical
    /// line (a single `Style` can't do that). `text` mirrors the plain concatenation so
    /// width math + tests still see the visible content.
    spans: Option<Vec<(String, Style)>>,
}

impl LogLine {
    /// A plain, unstyled line (draw-time content heuristic decides its color).
    fn plain(text: impl Into<String>) -> Self {
        LogLine { text: text.into(), style: None, spans: None }
    }
    /// A single-style line.
    fn styled(text: impl Into<String>, style: Style) -> Self {
        LogLine { text: text.into(), style: Some(style), spans: None }
    }
    /// A multi-span line; `text` is derived from the span texts for width math + tests.
    fn from_spans(spans: Vec<(String, Style)>) -> Self {
        let text: String = spans.iter().map(|(t, _)| t.as_str()).collect();
        LogLine { text, style: None, spans: Some(spans) }
    }
}

impl From<&str> for LogLine {
    fn from(s: &str) -> Self {
        LogLine { text: s.to_string(), style: None, spans: None }
    }
}

/// Run the cockpit to completion (blocks the calling thread). `to_main` carries the
/// window/quit signals to the main thread.
pub fn run(engine: Engine, to_main: Sender<UiToMain>) -> io::Result<()> {
    let mut term = setup_terminal()?;
    let res = run_loop(&mut term, &engine, &to_main);
    restore_terminal(&mut term)?;
    res
}

fn setup_terminal() -> io::Result<Term> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    // EnableMouseCapture so the OUTPUT/LOG pane is mouse-scrollable inside the alt screen.
    execute!(stdout, terminal::EnterAlternateScreen, EnableMouseCapture)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(term: &mut Term) -> io::Result<()> {
    terminal::disable_raw_mode()?;
    execute!(term.backend_mut(), DisableMouseCapture, terminal::LeaveAlternateScreen)?;
    term.show_cursor()
}

struct Cockpit {
    input: String,
    /// Cursor as a CHAR index into `input` (0..=char_count) — drives left/right/
    /// home/end navigation + cursor-aware insert/backspace/delete (was append-only).
    cursor: usize,
    log: Vec<LogLine>,
    history: Vec<String>,
    hist_idx: Option<usize>,
    /// On-disk history file (`$HOME/.trx64/history`), set at boot in [`run_loop`]. `None`
    /// when `$HOME` is unset — history then stays in-memory only. `new()` leaves it `None`
    /// so unit tests are side-effect-free.
    hist_path: Option<PathBuf>,
    snap: StateSnapshot,
    /// Lines scrolled UP from the bottom of the log (0 = pinned to the tail). Mouse
    /// wheel adjusts it; any new output snaps back to 0.
    scroll: usize,
    /// The OUTPUT/LOG pane's INNER width (border-subtracted) from the last [`draw_log`].
    /// The Enter handler reads it to render structured `help` output to the exact pane
    /// width. `Cell` so `draw_log` (which holds `&Cockpit`) can record it. `0` until the
    /// first draw — `format_help_lines` clamps a 0/tiny width to a usable minimum.
    log_inner_width: Cell<usize>,
}

impl Cockpit {
    fn new() -> Self {
        Self {
            scroll: 0,
            input: String::new(),
            cursor: 0,
            log: vec![
                "████████╗ ██████╗  ██╗  ██╗  ██████╗  ██╗  ██╗".into(),
                "╚══██╔══╝ ██╔══██╗ ╚██╗██╔╝ ██╔════╝  ██║  ██║".into(),
                "   ██║    ██████╔╝  ╚███╔╝  ███████╗  ███████║".into(),
                "   ██║    ██╔══██╗  ██╔██╗  ██╔═══██╗ ╚════██║".into(),
                "   ██║    ██║  ██║ ██╔╝ ██╗ ╚██████╔╝      ██║".into(),
                "   ╚═╝    ╚═╝  ╚═╝ ╚═╝  ╚═╝  ╚═════╝       ╚═╝".into(),
                "".into(),
                "powered on + running · a bare line → monitor (d/m/r/bk/g/trace) · /help · /quit"
                    .into(),
                "".into(),
            ],
            history: Vec::new(),
            hist_idx: None,
            hist_path: None,
            snap: StateSnapshot::default(),
            log_inner_width: Cell::new(0),
        }
    }

    // ── command-line editing (char-safe: cursor is a char index) ───────────────
    fn line_char_len(&self) -> usize {
        self.input.chars().count()
    }
    fn byte_at(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len())
    }
    fn insert_char(&mut self, c: char) {
        let b = self.byte_at(self.cursor);
        self.input.insert(b, c);
        self.cursor += 1;
    }
    fn backspace(&mut self) {
        if self.cursor > 0 {
            let b = self.byte_at(self.cursor - 1);
            self.input.remove(b);
            self.cursor -= 1;
        }
    }
    fn delete_at(&mut self) {
        if self.cursor < self.line_char_len() {
            let b = self.byte_at(self.cursor);
            self.input.remove(b);
        }
    }
    /// Replace the whole line (history recall / clear) and park the cursor at the end.
    fn set_line(&mut self, s: String) {
        self.cursor = s.chars().count();
        self.input = s;
    }

    // ── readline kill/word muscles (char-safe; cursor is a char index) ──────────
    /// Ctrl-K: drop everything from the cursor to the end of the line.
    fn kill_to_end(&mut self) {
        let b = self.byte_at(self.cursor);
        self.input.truncate(b);
    }
    /// Ctrl-U: drop everything before the cursor; the cursor moves to the start.
    fn kill_to_start(&mut self) {
        let b = self.byte_at(self.cursor);
        self.input.replace_range(0..b, "");
        self.cursor = 0;
    }
    /// Ctrl-W: delete the word before the cursor — first any whitespace directly to the
    /// left, then the run of non-whitespace (bash `unix-word-rubout`).
    fn delete_word_before(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let (b_start, b_end) = (self.byte_at(start), self.byte_at(self.cursor));
        self.input.replace_range(b_start..b_end, "");
        self.cursor = start;
    }

    /// Push `entry` onto the in-memory history, skipping a consecutive duplicate (bash
    /// `ignoredups`). Caps the ring to [`HIST_CAP`]. Returns whether the entry was pushed
    /// so the caller only appends genuinely-new lines to the on-disk history.
    fn push_history(&mut self, entry: &str) -> bool {
        if self.history.last().map(String::as_str) == Some(entry) {
            return false; // consecutive duplicate → keep the history clean
        }
        self.history.push(entry.to_string());
        if self.history.len() > HIST_CAP {
            let drop = self.history.len() - HIST_CAP;
            self.history.drain(0..drop);
        }
        true
    }

    fn push_log(&mut self, text: &str) {
        for line in text.split('\n') {
            self.log.push(LogLine::from(line));
        }
        self.trim_log();
    }

    /// Push already-styled lines (e.g. `!ls` filetype coloring, S4). Same tail-snap +
    /// bound as [`push_log`], but each line carries its own [`Style`].
    fn push_log_styled(&mut self, lines: Vec<LogLine>) {
        self.log.extend(lines);
        self.trim_log();
    }

    /// Snap the view to the tail and bound the log so it doesn't grow unbounded over a
    /// long session. Shared by [`push_log`] + [`push_log_styled`].
    fn trim_log(&mut self) {
        self.scroll = 0; // new output → snap to the tail
        const MAX: usize = 5000;
        if self.log.len() > MAX {
            let drop = self.log.len() - MAX;
            self.log.drain(0..drop);
        }
    }
}

// ── persistent command history ($HOME/.trx64/history) ──────────────────────────

/// The on-disk history file: `$HOME/.trx64/history`. `None` when `$HOME` is unset (the
/// cockpit then keeps history in-memory only).
fn history_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".trx64").join("history"))
}

/// Load history from `path`, one entry per line. Missing/unreadable file → empty (history
/// is best-effort, never fatal). Blank lines are dropped; the last [`HIST_CAP`] entries
/// are kept.
fn load_history_from(path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut lines: Vec<String> =
        content.lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
    if lines.len() > HIST_CAP {
        let drop = lines.len() - HIST_CAP;
        lines.drain(0..drop);
    }
    lines
}

/// Append one entry to the on-disk history, creating the parent dir. Best-effort: any I/O
/// error is swallowed so a read-only `$HOME` never breaks the cockpit. After the write the
/// file is compacted (see [`trim_history_file`]) so the persisted store stays bounded to
/// ~[`HIST_CAP`] and never grows without limit.
fn append_history_line(path: &Path, entry: &str) {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{entry}");
    }
    trim_history_file(path);
}

/// Rewrite `path` down to its last [`HIST_CAP`] non-blank lines once it has overshot the
/// cap by more than [`HIST_FILE_SLACK`]. This is what keeps the persisted history file
/// bounded (the in-memory ring and load path are capped separately). Best-effort: any I/O
/// error leaves the file as-is (it just stays a little larger until the next successful
/// compaction). The rewrite goes through a sibling temp file + rename so a crash mid-write
/// can't truncate an otherwise-good history.
fn trim_history_file(path: &Path) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    if lines.len() <= HIST_CAP + HIST_FILE_SLACK {
        return;
    }
    let mut body = lines[lines.len() - HIST_CAP..].join("\n");
    body.push('\n');
    let tmp = path.with_extension("history.tmp");
    if std::fs::write(&tmp, &body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn run_loop(term: &mut Term, engine: &Engine, to_main: &Sender<UiToMain>) -> io::Result<()> {
    let mut cp = Cockpit::new();
    // Load persistent history at boot (best-effort; empty when $HOME is unset/unreadable).
    cp.hist_path = history_path();
    if let Some(path) = &cp.hist_path {
        cp.history = load_history_from(path);
    }
    let poll = Duration::from_millis(50);
    let mut last_state = Instant::now();

    loop {
        // Refresh the live panel snapshot ~20 Hz (cheap dispatch read).
        if last_state.elapsed() >= Duration::from_millis(50) {
            cp.snap = engine.snapshot();
            last_state = Instant::now();
        }

        term.draw(|f| draw(f, &cp))?;

        if engine.should_quit() {
            let _ = to_main.send(UiToMain::Quit);
            return Ok(());
        }

        if event::poll(poll)? {
            match event::read()? {
                // Mouse wheel scrolls the OUTPUT/LOG pane (offset clamped in draw_log).
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp => cp.scroll = (cp.scroll + 3).min(cp.log.len()),
                    MouseEventKind::ScrollDown => cp.scroll = cp.scroll.saturating_sub(3),
                    _ => {}
                },
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    // Ctrl-<key> readline muscles. Handled here so they don't fall
                    // through to the Char insert arm (which ignores modifiers and would
                    // otherwise type the bare letter — the noted gotcha).
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        match key.code {
                            // Ctrl-C: clear a non-empty line (bash convention); an empty
                            // line quits.
                            XKeyCode::Char('c') => {
                                if cp.input.is_empty() {
                                    let _ = engine.exec_line("/quit");
                                    let _ = to_main.send(UiToMain::Quit);
                                    return Ok(());
                                }
                                cp.set_line(String::new());
                                cp.hist_idx = None;
                                continue;
                            }
                            // Ctrl-D: delete-at when the line is non-empty; an empty line
                            // quits (EOF).
                            XKeyCode::Char('d') => {
                                if cp.input.is_empty() {
                                    let _ = engine.exec_line("/quit");
                                    let _ = to_main.send(UiToMain::Quit);
                                    return Ok(());
                                }
                                cp.delete_at();
                                cp.hist_idx = None;
                                continue;
                            }
                            XKeyCode::Char('a') => {
                                cp.cursor = 0;
                                continue;
                            }
                            XKeyCode::Char('e') => {
                                cp.cursor = cp.line_char_len();
                                continue;
                            }
                            XKeyCode::Char('k') => {
                                cp.kill_to_end();
                                cp.hist_idx = None;
                                continue;
                            }
                            XKeyCode::Char('u') => {
                                cp.kill_to_start();
                                cp.hist_idx = None;
                                continue;
                            }
                            XKeyCode::Char('w') => {
                                cp.delete_word_before();
                                cp.hist_idx = None;
                                continue;
                            }
                            // Ctrl-L: clear the log pane; the next draw repaints.
                            XKeyCode::Char('l') => {
                                cp.log.clear();
                                cp.scroll = 0;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    match key.code {
                        // Tab: namespace-aware completion (verbs in /, !, and the bare
                        // monitor namespace; paths for path-taking verbs). Parks the
                        // cursor at the end + pushes any candidate list itself.
                        XKeyCode::Tab => {
                            autocomplete(&mut cp, engine);
                        }
                        XKeyCode::Char(c) => {
                            cp.insert_char(c);
                            cp.hist_idx = None;
                        }
                        XKeyCode::Backspace => {
                            cp.backspace();
                            cp.hist_idx = None; // editing a recalled line detaches it
                        }
                        XKeyCode::Delete => {
                            cp.delete_at();
                            cp.hist_idx = None; // editing a recalled line detaches it
                        }
                        XKeyCode::Left => {
                            cp.cursor = cp.cursor.saturating_sub(1);
                        }
                        XKeyCode::Right => {
                            cp.cursor = (cp.cursor + 1).min(cp.line_char_len());
                        }
                        XKeyCode::Home => {
                            cp.cursor = 0;
                        }
                        XKeyCode::End => {
                            cp.cursor = cp.line_char_len();
                        }
                        XKeyCode::Up => {
                            if !cp.history.is_empty() {
                                let i = match cp.hist_idx {
                                    None => cp.history.len() - 1,
                                    Some(0) => 0,
                                    Some(i) => i - 1,
                                };
                                cp.hist_idx = Some(i);
                                cp.set_line(cp.history[i].clone());
                            }
                        }
                        XKeyCode::Down => {
                            if let Some(i) = cp.hist_idx {
                                if i + 1 < cp.history.len() {
                                    cp.hist_idx = Some(i + 1);
                                    cp.set_line(cp.history[i + 1].clone());
                                } else {
                                    cp.hist_idx = None;
                                    cp.set_line(String::new());
                                }
                            }
                        }
                        XKeyCode::Enter => {
                            let line = cp.input.trim().to_string();
                            cp.set_line(String::new());
                            cp.hist_idx = None;
                            if line.is_empty() {
                                continue;
                            }
                            // Dedup consecutive duplicates; only genuinely-new lines get
                            // appended to the on-disk history.
                            if cp.push_history(&line) {
                                if let Some(path) = cp.hist_path.as_deref() {
                                    append_history_line(path, &line);
                                }
                            }
                            cp.push_log(&format!("> {line}"));
                            let r = engine.exec_line(&line);
                            if !r.output.is_empty() {
                                // S10: every help variant is re-rendered structured
                                // (colored headers, aligned verb column, hanging-indent-
                                // wrapped descriptions) to the pane width via
                                // `format_help_lines`. `help`/`?`/`!help` return the
                                // daemon's VICE-superset monitor reference
                                // (`monitor_help_text`, main.rs); `/help` returns the CLI's
                                // OWN cockpit help (`engine::help_text`) — a different,
                                // shorter text the monitor-tuned formatter still renders
                                // acceptably. S4: `!ls`/`!dir` output is filetype-colored
                                // per entry; every other command's output logs verbatim.
                                if is_help_command(&line) {
                                    let width = cp.log_inner_width.get();
                                    cp.push_log_styled(format_help_lines(&r.output, width));
                                } else {
                                    match ls_styled_lines(&line, &r.output) {
                                        Some(styled) => cp.push_log_styled(styled),
                                        None => cp.push_log(&r.output),
                                    }
                                }
                            }
                            if r.open_window {
                                let _ = to_main.send(UiToMain::OpenWindow);
                            }
                            if r.quit {
                                let _ = to_main.send(UiToMain::Quit);
                                return Ok(());
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

// ── Tab completion (namespace-aware: /-verbs, !-verbs, bare monitor verbs, paths) ──

/// VM (`/`) verbs — the machine namespace, INCLUDING the aliases `exec_line` accepts
/// (umount/undump/joy/exit), so Tab offers every spelling.
const VM_VERBS: &[&str] = &[
    "power", "reset", "run", "pause", "step", "mount", "eject", "umount", "load", "warp",
    "joystick", "joy", "window", "dump", "restore", "undump", "ringdump", "ringload", "settings",
    "help", "quit", "exit",
];

/// VM verbs whose argument is a path — after `"/<verb> "`, Tab path-completes the last
/// token (mirrors the PATH verbs in `engine::exec_line`).
const VM_PATH_VERBS: &[&str] =
    &["mount", "load", "run", "dump", "restore", "undump", "ringdump", "ringload"];

/// FS (`!`) verbs — the filesystem namespace (mirrors `engine::FS_VERBS`, the monitor
/// file shell verbs, re-prefixed with `!`).
const FS_VERBS: &[&str] =
    &["pwd", "cd", "ls", "dir", "mkdir", "rmdir", "load", "save", "bload", "bsave"];

/// FS verbs whose argument is a path (all of them except `pwd`).
const FS_PATH_VERBS: &[&str] =
    &["cd", "ls", "dir", "mkdir", "rmdir", "load", "save", "bload", "bsave"];

/// Curated monitor verbs (the bare namespace) — from `MONITOR.md`, minus the FS verbs
/// (those live behind `!`). Used for bare-line verb completion.
const MONITOR_VERBS: &[&str] = &[
    // execution
    "g", "x", "until", "z", "step", "n", "next", "ret", "return", "focus", "sf", "nf", "flow",
    "bt", "reset",
    // memory
    "m", "d", "sd", "df", "screen", "io", "bitmap", "bank", "wr", "f", "a", "t", "c", "h",
    // breakpoints & observers
    "bk", "del", "obs", "ignore",
    // cpu
    "r", "sidefx", "device",
    // state & trace
    "dump", "undump", "savecrt", "swapcrt", "trace", "tracedb", "traceindex",
    // analysis
    "map", "taint", "swimlane", "chis",
    // reverse-debug
    "rstep", "reverse", "whowrote", "triage", "revdepth", "diff", "ringdump", "ringload",
    // knowledge
    "inspect", "xref", "sym",
];

/// What a Tab press should complete for the current input line. Pure classification —
/// no I/O — so it is unit-testable without the rpc.
enum CompletePlan {
    /// Nothing to complete (empty bare line; a non-path verb followed by args).
    Nothing,
    /// Complete a verb from `set`, displayed with the namespace prefix `ns` (`/`/`!`/``).
    Verbs { ns: &'static str, stem: String, set: &'static [&'static str] },
    /// Complete a path — the last token of the line is a path argument.
    Path,
}

/// Decide what Tab completes for `input`, by namespace. Pure.
fn plan_complete(input: &str) -> CompletePlan {
    // `/` — the machine namespace.
    if let Some(rest) = input.strip_prefix('/') {
        if !rest.contains(' ') {
            return CompletePlan::Verbs { ns: "/", stem: rest.to_string(), set: VM_VERBS };
        }
        let verb = rest.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        return if VM_PATH_VERBS.contains(&verb.as_str()) {
            CompletePlan::Path
        } else {
            CompletePlan::Nothing
        };
    }
    // `!` — the filesystem namespace.
    if let Some(rest) = input.strip_prefix('!') {
        if !rest.contains(' ') {
            return CompletePlan::Verbs { ns: "!", stem: rest.to_string(), set: FS_VERBS };
        }
        let verb = rest.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        return if FS_PATH_VERBS.contains(&verb.as_str()) {
            CompletePlan::Path
        } else {
            CompletePlan::Nothing
        };
    }
    // bare — the monitor namespace. Verb completion only (no space yet); a space means
    // args (addresses / symbols), which are out of scope for now.
    if !input.is_empty() && !input.contains(' ') {
        return CompletePlan::Verbs { ns: "", stem: input.to_string(), set: MONITOR_VERBS };
    }
    CompletePlan::Nothing
}

/// Tab-complete the cockpit input line in place, namespace-aware. Fills the line (verb
/// or path), pushes an ambiguous-verb candidate list or a COLORED path-candidate list
/// into the log, and parks the cursor at the end.
fn autocomplete(cp: &mut Cockpit, engine: &Engine) {
    match plan_complete(&cp.input) {
        CompletePlan::Nothing => {}
        CompletePlan::Verbs { ns, stem, set } => {
            let (line, listing) = complete_verb(ns, &stem, set);
            cp.input = line;
            if !listing.is_empty() {
                cp.push_log_styled(listing);
            }
        }
        CompletePlan::Path => {
            let (line, listing) = complete_path(&cp.input, engine);
            cp.input = line;
            if !listing.is_empty() {
                cp.push_log_styled(listing);
            }
        }
    }
    cp.cursor = cp.line_char_len();
}

/// Pure verb completion. Returns the (possibly completed) line and the candidate lines
/// to log — empty on a zero/unique match; one plain line listing the candidates when the
/// prefix is ambiguous (the line is also filled to the longest common prefix).
fn complete_verb(ns: &str, stem: &str, set: &[&str]) -> (String, Vec<LogLine>) {
    let matches: Vec<&str> = set.iter().copied().filter(|v| v.starts_with(stem)).collect();
    match matches.as_slice() {
        [] => (format!("{ns}{stem}"), Vec::new()),
        [only] => (format!("{ns}{only} "), Vec::new()),
        many => {
            let lcp = longest_common_prefix(many);
            let line =
                if lcp.len() > stem.len() { format!("{ns}{lcp}") } else { format!("{ns}{stem}") };
            let listing = format!(
                "  {}",
                many.iter().map(|m| format!("{ns}{m}")).collect::<Vec<_>>().join("  ")
            );
            (line, vec![LogLine::from(listing.as_str())])
        }
    }
}

/// The path token being completed, carved out of the input line.
#[derive(Debug, Clone, PartialEq)]
struct PathTok {
    /// The untouched head of the line before the token (INCLUDING the separating space,
    /// but NOT an opening quote — the quote is captured by `quoted`).
    head: String,
    /// The partial path typed so far (may contain spaces when it was quoted).
    partial: String,
    /// Whether the token was introduced by an (unclosed) double-quote.
    quoted: bool,
}

/// Extract the path token from `input`: everything after an unclosed `"` (a quoted path,
/// spaces allowed), else the last whitespace-delimited token. Pure.
fn split_path_token(input: &str) -> PathTok {
    // An odd number of double-quotes → the last `"` is still open: everything after it
    // is the (space-allowing) path token.
    if input.matches('"').count() % 2 == 1 {
        let q = input.rfind('"').unwrap();
        return PathTok {
            head: input[..q].to_string(),
            partial: input[q + 1..].to_string(),
            quoted: true,
        };
    }
    match input.rfind(char::is_whitespace) {
        Some(i) => {
            let next = i + input[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            PathTok {
                head: input[..next].to_string(),
                partial: input[next..].to_string(),
                quoted: false,
            }
        }
        None => PathTok { head: String::new(), partial: input.to_string(), quoted: false },
    }
}

/// Pure line reconstruction after a path lookup. `single` is `Some(is_dir)` when exactly
/// one candidate matched (fill + terminate: dir → `/`, file → space, closing the quote
/// when needed); `None` when many matched (fill the common prefix only, token left open).
/// (Re)quotes when the token was already quoted or the completed text contains a space.
fn apply_path_completion(tok: &PathTok, common: &str, single: Option<bool>) -> String {
    // The daemon's `common` is relative to the token's directory part (it splits at the
    // last `/`), so re-attach that prefix — computed identically — to rebuild the token.
    let dir_part = match tok.partial.rfind('/') {
        Some(i) => &tok.partial[..=i],
        None => "",
    };
    let completed = format!("{dir_part}{common}");
    let needs_quote = tok.quoted || completed.contains(' ');
    let open = if needs_quote { "\"" } else { "" };
    let tail = match single {
        Some(true) => "/".to_string(),
        Some(false) => {
            if needs_quote {
                "\" ".to_string()
            } else {
                " ".to_string()
            }
        }
        None => String::new(),
    };
    format!("{}{open}{completed}{tail}", tok.head)
}

/// Path completion via the daemon `fs/complete` rpc. Returns the (possibly completed)
/// line and, on multiple candidates, a COLORED candidate list to log. A soft/empty rpc
/// result leaves the line untouched.
fn complete_path(input: &str, engine: &Engine) -> (String, Vec<LogLine>) {
    let tok = split_path_token(input);
    let resp = engine.rpc("fs/complete", json!({ "partial": tok.partial })).unwrap_or(Value::Null);
    let entries = resp.get("entries").and_then(|e| e.as_array()).cloned().unwrap_or_default();
    let common = resp.get("common").and_then(|c| c.as_str()).unwrap_or("");
    match entries.len() {
        0 => (input.to_string(), Vec::new()),
        1 => {
            let is_dir = entries[0].get("is_dir").and_then(|d| d.as_bool()).unwrap_or(false);
            (apply_path_completion(&tok, common, Some(is_dir)), Vec::new())
        }
        _ => (apply_path_completion(&tok, common, None), candidate_log_lines(&entries)),
    }
}

/// One colored [`LogLine`] per candidate (each line carries a single [`Style`], so
/// per-filetype coloring is one entry per line), capped so a large directory can't flood
/// the log.
fn candidate_log_lines(entries: &[Value]) -> Vec<LogLine> {
    const CAP: usize = 100;
    let mut out: Vec<LogLine> = entries
        .iter()
        .take(CAP)
        .map(|e| {
            let name = e.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let is_dir = e.get("is_dir").and_then(|d| d.as_bool()).unwrap_or(false);
            let text = if is_dir { format!("  {name}/") } else { format!("  {name}") };
            LogLine { text, style: Some(crate::ftcolor::style_for(name, is_dir)), spans: None }
        })
        .collect();
    if entries.len() > CAP {
        out.push(LogLine::from(format!("  … {} more", entries.len() - CAP).as_str()));
    }
    out
}

/// The filetype [`Style`] for a single `ls`/`dir` entry line. The daemon FS format is
/// `"  {d|-} {name}"` (main.rs ls verb): a two-space indent, a `d`/`-` dir flag, a
/// space, then the name. Returns `None` for the `"{dir}:"` header, the `"  (empty)"`
/// sentinel, and anything that doesn't match the entry shape — those stay plain.
fn ls_entry_style(line: &str) -> Option<Style> {
    let rest = line.strip_prefix("  ")?;
    let bytes = rest.as_bytes();
    let is_dir = match bytes.first()? {
        b'd' => true,
        b'-' => false,
        _ => return None, // header path / "(empty)" / anything else → plain
    };
    if bytes.get(1) != Some(&b' ') {
        return None;
    }
    // Bytes 0 (flag) + 1 (space) are ASCII, so slicing at 2 is a valid char boundary.
    let name = &rest[2..];
    Some(crate::ftcolor::style_for(name, is_dir))
}

/// If `line` is an `!ls`/`!dir` cockpit command, split its `output` into per-line
/// [`LogLine`]s with filetype coloring (entries via [`ls_entry_style`]; header +
/// `(empty)` sentinel left plain). Returns `None` for any other command so its output
/// is logged verbatim. Pure — no I/O, so the routing is unit-testable.
fn ls_styled_lines(line: &str, output: &str) -> Option<Vec<LogLine>> {
    let fs = line.strip_prefix('!')?.trim_start();
    let verb = fs.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    if verb != "ls" && verb != "dir" {
        return None;
    }
    Some(
        output
            .split('\n')
            .map(|l| LogLine { text: l.to_string(), style: ls_entry_style(l), spans: None })
            .collect(),
    )
}

fn longest_common_prefix(items: &[&str]) -> String {
    let first = items.first().copied().unwrap_or("");
    let mut len = first.len();
    for s in &items[1..] {
        let common = first.bytes().zip(s.bytes()).take_while(|(a, b)| a == b).count();
        len = len.min(common);
    }
    first[..len].to_string()
}

fn draw(f: &mut Frame, cp: &Cockpit) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // gauges row
            Constraint::Length(3), // flow/vectors
            Constraint::Min(3),    // log
            Constraint::Length(3), // input
        ])
        .split(area);

    draw_gauges(f, rows[0], &cp.snap);
    draw_flow(f, rows[1], &cp.snap);
    draw_log(f, rows[2], cp);
    draw_input(f, rows[3], cp);
}

fn draw_gauges(f: &mut Frame, area: Rect, s: &StateSnapshot) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(28),
            Constraint::Percentage(32),
        ])
        .split(area);

    // CPU panel
    let cpu = vec![
        Line::from(vec![
            Span::styled("PC ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("${:04X}", s.pc), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled("SP ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("${:02X}", s.sp), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("A ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("${:02X}", s.a), Style::default().fg(Color::White)),
            Span::styled("  X ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("${:02X}", s.x), Style::default().fg(Color::White)),
            Span::styled("  Y ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("${:02X}", s.y), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("P ", Style::default().fg(Color::DarkGray)),
            Span::styled(s.flags_str(), Style::default().fg(Color::Yellow)),
        ]),
    ];
    f.render_widget(panel(cpu, "CPU"), cols[0]);

    // MACHINE panel
    let (run_label, run_color) = if s.running {
        ("● RUNNING", Color::Green)
    } else {
        ("■ PAUSED", Color::Red)
    };
    let warp_label = if s.warp { "WARP 8×" } else { "PAL 1×" };
    let machine = vec![
        Line::from(Span::styled(run_label, Style::default().fg(run_color).add_modifier(Modifier::BOLD))),
        Line::from(vec![
            Span::styled("pacing ", Style::default().fg(Color::DarkGray)),
            Span::styled(warp_label, Style::default().fg(Color::Magenta)),
        ]),
        Line::from(vec![
            Span::styled("cyc ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", s.c64_cycles), Style::default().fg(Color::White)),
        ]),
    ];
    f.render_widget(panel(machine, "MACHINE"), cols[1]);

    // VIC panel
    let vic = vec![
        Line::from(vec![
            Span::styled("raster ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:>3}:{:<2}", s.raster_line, s.raster_cycle), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("mode ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", s.vic_mode), Style::default().fg(Color::White)),
            Span::styled("  bg ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", s.background), Style::default().fg(Color::White)),
            Span::styled("  bdr ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", s.border), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("drive ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", s.drive_cycles), Style::default().fg(Color::White)),
        ]),
    ];
    f.render_widget(panel(vic, "VIC"), cols[2]);
}

fn draw_flow(f: &mut Frame, area: Rect, s: &StateSnapshot) {
    let stop = s.stop_reason.as_deref().unwrap_or("—");
    let line = Line::from(vec![
        Span::styled("IRQ ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("${:04X}", s.irq_vec), Style::default().fg(Color::Blue)),
        Span::styled("  NMI ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("${:04X}", s.nmi_vec), Style::default().fg(Color::Blue)),
        Span::styled("  stop ", Style::default().fg(Color::DarkGray)),
        Span::styled(stop.to_string(), Style::default().fg(Color::Yellow)),
        Span::styled("  flow ", Style::default().fg(Color::DarkGray)),
        Span::styled("main", Style::default().fg(Color::Gray)),
    ]);
    f.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title(" FLOW / VECTORS ")),
        area,
    );
}

fn draw_log(f: &mut Frame, area: Rect, cp: &Cockpit) {
    let inner_h = area.height.saturating_sub(2) as usize;
    // Record the inner (border-subtracted) width so the Enter handler can width-wrap
    // structured `help` output to exactly what this pane renders (S10).
    cp.log_inner_width.set(area.width.saturating_sub(2) as usize);
    // Scroll: `cp.scroll` lines up from the tail, clamped so the window stays in range.
    let max_scroll = cp.log.len().saturating_sub(inner_h);
    let scroll = cp.scroll.min(max_scroll);
    let end = cp.log.len() - scroll;
    let start = end.saturating_sub(inner_h);
    let lines: Vec<Line> = cp.log[start..end]
        .iter()
        .map(|l| {
            if let Some(spans) = &l.spans {
                // Multi-span line (e.g. structured `help`: green verb + default desc, S10).
                Line::from(
                    spans.iter().map(|(t, s)| Span::styled(t.clone(), *s)).collect::<Vec<_>>(),
                )
            } else if let Some(style) = l.style {
                // Pre-styled line (e.g. `!ls` filetype coloring, S4).
                Line::from(Span::styled(l.text.clone(), style))
            } else if l.text.starts_with("> ") {
                Line::from(Span::styled(l.text.clone(), Style::default().fg(Color::Green)))
            } else if l.text.contains('█') {
                // The TRX64 startup banner.
                Line::from(Span::styled(
                    l.text.clone(),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(Span::raw(l.text.clone()))
            }
        })
        .collect();
    // Title shows a scrollback indicator when not pinned to the tail.
    let title = if scroll > 0 {
        format!(" OUTPUT / LOG  ▲ {scroll} (scroll down to live) ")
    } else {
        " OUTPUT / LOG ".to_string()
    };
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_input(f: &mut Frame, area: Rect, cp: &Cockpit) {
    // Block cursor AT the char index (reverse-video over the char under it; a green
    // block past the end) so the cursor can sit mid-string for left/right editing.
    let chars: Vec<char> = cp.input.chars().collect();
    let cur = cp.cursor.min(chars.len());
    let before: String = chars[..cur].iter().collect();
    let (under, after): (String, String) = if cur < chars.len() {
        (chars[cur].to_string(), chars[cur + 1..].iter().collect())
    } else {
        (" ".to_string(), String::new())
    };
    let line = Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw(before),
        Span::styled(under, Style::default().fg(Color::Green).add_modifier(Modifier::REVERSED)),
        Span::raw(after),
    ]);
    f.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title(" command ")),
        area,
    );
}

fn panel<'a>(lines: Vec<Line<'a>>, title: &'a str) -> Paragraph<'a> {
    Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(format!(" {title} ")))
}

// ── S10: structured, colored, width-aware INLINE help rendering ─────────────────
//
// The bare `help` verb (and its `?`/`!help` aliases) returns the big VICE-superset monitor
// reference — dozens of `verb  description` rows organised under section headers. (`/help`
// is the ONE exception: it returns the CLI's own, shorter cockpit help — `engine::help_text`
// — not the monitor reference; it still flows through the same formatter below.) Dumped
// verbatim into the OUTPUT/LOG pane, long descriptions wrap and shatter the verb column
// into an unreadable grey blob. `format_help_lines` re-renders that text IN PLACE (no
// pager): colored section headers, the verb column aligned + green, and descriptions
// hanging-indent-wrapped to the pane width. The parser degrades gracefully — any line it
// doesn't recognise renders as plain text; it never panics, at any width.

/// The visual indent (chars) of a monitor verb row.
const HELP_VERB_INDENT: usize = 4;
/// Cap for the aligned verb column. A verb longer than this is emitted on its own line(s)
/// with the description indented underneath (an over-wide column would waste the pane).
const HELP_VERB_CAP: usize = 18;
/// The gap between the verb column and the description column.
const HELP_GAP: usize = 2;
/// The indent (chars) at which a sub / continuation line hangs.
const HELP_SUB_INDENT: usize = 6;
/// Floor for the render width so a 0/1/tiny pane still produces sane, panic-free output.
const HELP_MIN_WIDTH: usize = 10;
/// How many chars past the aligned description column a single-space verb row's args may
/// overflow before its trailing text is treated as more spec syntax rather than a
/// description. Keeps `d [lens] [a] [end] disassemble …` (args 2 chars over the column) a
/// Verb, while leaving the long `obs …` spec (whose next space is ~14 chars past the
/// column) a Spec. See [`classify_help_line`].
const HELP_SINGLE_SPACE_SLOP: usize = 3;

/// Whether `line` (already trimmed, mirroring `exec_line`'s lowercasing) is a help command
/// whose output should go through [`format_help_lines`]. Pure.
fn is_help_command(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "help" | "?" | "/help" | "!help")
}

/// A classified source line of help text. Borrows the trimmed content from the input.
#[derive(Debug, PartialEq)]
enum HelpLine<'a> {
    /// A blank separator line.
    Blank,
    /// The col-0 title line (e.g. `monitor (VICE-superset):`).
    Title(&'a str),
    /// A 2-space section header with no `verb  desc` split (e.g. `EXEC`).
    Section(&'a str),
    /// A verb row split on the first run of >=2 spaces into `verb` + `desc`.
    Verb { verb: &'a str, desc: &'a str },
    /// A verb/spec row with no description (all single spaces, e.g. the `obs …` spec).
    Spec(&'a str),
    /// A sub / continuation line (6+-space indent) — a dim continuation of the row above.
    Sub(&'a str),
}

/// Classify one raw help line by its leading indent + shape. Pure. Order matters: a deep
/// (6+) indent is a continuation EVEN when it happens to contain a 2-space run (e.g. the
/// `log fields: …  e.g. …` sub-line), so the indent test precedes the verb/desc split.
///
/// `aligned_desc_col` is the source column at which descriptions line up across verb rows
/// (from [`detect_help_desc_col`] over the whole help text) — `None` when there are no verb
/// rows to align to. It lets an indent 3..=5 row whose verb+args column is so wide that only
/// a SINGLE space separates it from the description (e.g. `m [lens] <a> [b] memory dump …`)
/// still classify as [`HelpLine::Verb`] instead of a fully-green [`HelpLine::Spec`].
fn classify_help_line(line: &str, aligned_desc_col: Option<usize>) -> HelpLine<'_> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    // Leading spaces are ASCII, so `indent` is a valid char boundary + a char count.
    let content = line[indent..].trim_end();
    if content.is_empty() {
        return HelpLine::Blank;
    }
    if indent == 0 {
        return HelpLine::Title(content);
    }
    if indent >= HELP_SUB_INDENT {
        return HelpLine::Sub(content);
    }
    // indent 1..=5: a run of >=2 spaces splits verb | description.
    if let Some(pos) = content.find("  ") {
        let verb = content[..pos].trim_end();
        let desc = content[pos..].trim_start();
        if !verb.is_empty() && !desc.is_empty() {
            return HelpLine::Verb { verb, desc };
        }
    }
    if indent <= 2 {
        HelpLine::Section(content)
    } else {
        // indent 3..=5 with no >=2-space run. A column-aligned verb row can still leave only
        // a SINGLE space between a full-width verb+args column and its description — e.g.
        // `m [lens] <a> [b] memory dump …`, `d [lens] [a] [end] disassemble …`, and
        // `bk <a> | bk -<a> set / remove …` (the three most-used monitor commands). Recover
        // those via the aligned description column: if a single space sits right at the
        // column (a verb that exactly fills it) or a few chars past it (args that overflow
        // it), split there into verb | description. Genuinely desc-less specs (`obs …`,
        // `ignore …`) carry NO space near the column, so they stay Spec (rendered fully
        // green). Verb/args + the gap are ASCII, so byte offsets == char offsets here.
        if let Some(col) = aligned_desc_col {
            let lo = col.saturating_sub(1);
            let hi = (lo + HELP_SINGLE_SPACE_SLOP).min(content.len());
            let bytes = content.as_bytes();
            if let Some(pos) = (lo..=hi).find(|&p| bytes.get(p) == Some(&b' ')) {
                let verb = content[..pos].trim_end();
                let desc = content[pos + 1..].trim_start();
                if !verb.is_empty() && !desc.is_empty() {
                    return HelpLine::Verb { verb, desc };
                }
            }
        }
        // No description column to align to → a full spec (no description column).
        HelpLine::Spec(content)
    }
}

/// The source column at which descriptions line up across the help's verb rows — the mode of
/// the "end of the first run of >=2 spaces" position over indent 3..=5 rows (i.e. where the
/// padded verb column ends and the description begins). Ties break to the SMALLEST column so
/// the single-space fallback in [`classify_help_line`] stays conservative. `None` when no
/// such rows exist. Pure. (Verb columns + the gap are ASCII, so this byte offset is also a
/// char offset.)
fn detect_help_desc_col(raw: &str) -> Option<usize> {
    use std::collections::HashMap;
    let mut hist: HashMap<usize, usize> = HashMap::new();
    for line in raw.split('\n') {
        let indent = line.len() - line.trim_start_matches(' ').len();
        if !(3..=5).contains(&indent) {
            continue;
        }
        let content = line[indent..].trim_end();
        if let Some(start) = content.find("  ") {
            // End of the space run = the column where the description begins.
            let end = content[start..]
                .find(|c: char| c != ' ')
                .map(|off| start + off)
                .unwrap_or(content.len());
            *hist.entry(end).or_insert(0) += 1;
        }
    }
    // Most frequent column wins; on a tie, the smaller (leftmost) column.
    hist.into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(col, _)| col)
}

/// Word-wrap `text` to lines of at most `width` chars, char-safe. Word-aware; a single
/// word longer than `width` is hard-split on char boundaries (never mid-codepoint). Always
/// returns >=1 line (empty/blank input → one empty line). `width` is floored to 1.
fn wrap_help_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize; // chars in `cur`
    for word in text.split_whitespace() {
        let wlen = word.chars().count();
        // Flush if appending (with a joining space) would overflow the line.
        if cur_len > 0 && cur_len + 1 + wlen > width {
            lines.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        if wlen <= width {
            if cur_len > 0 {
                cur.push(' ');
                cur_len += 1;
            }
            cur.push_str(word);
            cur_len += wlen;
        } else {
            // Word wider than a whole line: emit char-boundary chunks; the tail stays as
            // the current line so later words can still pack onto it.
            if cur_len > 0 {
                lines.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            let chars: Vec<char> = word.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let end = (i + width).min(chars.len());
                let chunk: String = chars[i..end].iter().collect();
                if end == chars.len() {
                    cur = chunk;
                    cur_len = end - i;
                } else {
                    lines.push(chunk);
                }
                i = end;
            }
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Push `text` wrapped with a hanging indent: the first line sits at `indent`, wrapped
/// continuation lines sit two chars deeper. Every emitted line is `<= width` chars. All
/// lines carry `style`. Char-safe + panic-free at any width.
fn push_hanging(out: &mut Vec<LogLine>, text: &str, indent: usize, width: usize, style: Style) {
    let indent = indent.min(width.saturating_sub(1));
    let cont = (indent + 2).min(width.saturating_sub(1)).max(indent);
    // Wrap to the (narrower) continuation width so BOTH the first + hanging lines fit.
    let seg_w = width.saturating_sub(cont).max(1);
    for (i, seg) in wrap_help_text(text, seg_w).into_iter().enumerate() {
        let pad = if i == 0 { indent } else { cont };
        out.push(LogLine::styled(format!("{}{}", " ".repeat(pad), seg), style));
    }
}

/// Render the raw help `raw` into styled [`LogLine`]s wrapped to `width` (the log pane's
/// inner width). PURE. See the module comment above for the layout contract.
fn format_help_lines(raw: &str, width: usize) -> Vec<LogLine> {
    let title_style = Style::default().add_modifier(Modifier::BOLD);
    let section_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let verb_style = Style::default().fg(Color::Green);
    let desc_style = Style::default(); // default foreground
    let sub_style = Style::default().fg(Color::DarkGray);

    let width = width.max(HELP_MIN_WIDTH);

    // The source column descriptions align to, so single-space verb rows (a verb+args that
    // fills the column, leaving only ONE space before the description) still classify as Verb.
    let aligned_desc_col = detect_help_desc_col(raw);
    let classified: Vec<HelpLine> =
        raw.split('\n').map(|l| classify_help_line(l, aligned_desc_col)).collect();

    // The aligned verb column: the longest verb, capped. (`Verb` rows only — specs/subs
    // don't share the column.)
    let max_verb = classified
        .iter()
        .filter_map(|l| match l {
            HelpLine::Verb { verb, .. } => Some(verb.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let verb_col = max_verb.min(HELP_VERB_CAP);

    // Column where descriptions begin. Clamped so at least 1 char of description fits even
    // on a tiny pane (`desc_col <= width-1`).
    let desc_col = (HELP_VERB_INDENT + verb_col + HELP_GAP).min(width.saturating_sub(1));
    // Char width available to pad the verb before the gap. When clamped tight this shrinks,
    // pushing more verbs onto their own line — still correct, just less pretty.
    let verb_pad_target = desc_col.saturating_sub(HELP_VERB_INDENT + HELP_GAP);
    let desc_width = width.saturating_sub(desc_col).max(1);

    let mut out: Vec<LogLine> = Vec::new();
    for hl in classified {
        match hl {
            HelpLine::Blank => out.push(LogLine::plain("")),
            HelpLine::Title(t) => push_hanging(&mut out, t, 0, width, title_style),
            HelpLine::Section(s) => push_hanging(&mut out, s, 2, width, section_style),
            HelpLine::Spec(spec) => {
                push_hanging(&mut out, spec, HELP_VERB_INDENT, width, verb_style)
            }
            HelpLine::Sub(sub) => push_hanging(&mut out, sub, HELP_SUB_INDENT, width, sub_style),
            HelpLine::Verb { verb, desc } => {
                let vlen = verb.chars().count();
                let desc_segs = wrap_help_text(desc, desc_width);
                if verb_pad_target > 0 && vlen <= verb_pad_target {
                    // Inline: `<indent><verb (green)><pad><desc-seg-0 (default)>`, then any
                    // wrapped continuation lines aligned under the description column.
                    let pad = verb_pad_target - vlen + HELP_GAP;
                    let first = desc_segs.first().cloned().unwrap_or_default();
                    out.push(LogLine::from_spans(vec![
                        (" ".repeat(HELP_VERB_INDENT), desc_style),
                        (verb.to_string(), verb_style),
                        (" ".repeat(pad), desc_style),
                        (first, desc_style),
                    ]));
                    for seg in desc_segs.iter().skip(1) {
                        out.push(LogLine::styled(
                            format!("{}{}", " ".repeat(desc_col), seg),
                            desc_style,
                        ));
                    }
                } else {
                    // Verb wider than the column (or a too-tight pane): verb on its own
                    // line(s), the description indented underneath at the desc column.
                    push_hanging(&mut out, verb, HELP_VERB_INDENT, width, verb_style);
                    for seg in &desc_segs {
                        out.push(LogLine::styled(
                            format!("{}{}", " ".repeat(desc_col), seg),
                            desc_style,
                        ));
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_editor_cursor_insert_backspace_delete() {
        let mut cp = Cockpit::new();
        for c in "abc".chars() {
            cp.insert_char(c);
        }
        assert_eq!(cp.input, "abc");
        assert_eq!(cp.cursor, 3);
        // mid-string insert: a|bc → aX|bc
        cp.cursor = 1;
        cp.insert_char('X');
        assert_eq!(cp.input, "aXbc");
        assert_eq!(cp.cursor, 2);
        // backspace removes the char BEFORE the cursor: aX|bc → a|bc
        cp.backspace();
        assert_eq!(cp.input, "abc");
        assert_eq!(cp.cursor, 1);
        // delete removes the char AT the cursor: a|bc → a|c
        cp.delete_at();
        assert_eq!(cp.input, "ac");
        assert_eq!(cp.cursor, 1);
        // navigation clamps; set_line parks the cursor at the end.
        cp.cursor = 99;
        cp.cursor = cp.cursor.min(cp.line_char_len());
        assert_eq!(cp.cursor, 2);
        cp.set_line("hello".into());
        assert_eq!((cp.input.as_str(), cp.cursor), ("hello", 5));
        // backspace/delete at the edges are no-ops, not panics.
        cp.cursor = 0;
        cp.backspace();
        cp.cursor = cp.line_char_len();
        cp.delete_at();
        assert_eq!(cp.input, "hello");
    }

    // ── S5: namespace-aware Tab completion ─────────────────────────────────────

    #[test]
    fn verb_single_completes_with_space() {
        let (line, listing) = complete_verb("/", "pow", VM_VERBS);
        assert_eq!(line, "/power ");
        assert!(listing.is_empty());
    }

    #[test]
    fn verb_ambiguous_fills_common_prefix_and_lists() {
        // /re → {reset, restore}; LCP = "res".
        let (line, listing) = complete_verb("/", "re", VM_VERBS);
        assert_eq!(line, "/res");
        assert_eq!(listing.len(), 1);
        let msg = &listing[0].text;
        assert!(msg.contains("/reset") && msg.contains("/restore"), "listing: {msg}");
        // the candidate list is a plain line (no per-entry style for verbs)
        assert_eq!(listing[0].style, None);
    }

    #[test]
    fn verb_no_match_leaves_line_unchanged() {
        let (line, listing) = complete_verb("/", "zzz", VM_VERBS);
        assert_eq!(line, "/zzz");
        assert!(listing.is_empty());
    }

    #[test]
    fn plan_routes_each_namespace() {
        // verb completion in each namespace
        assert!(matches!(plan_complete("/mo"), CompletePlan::Verbs { ns: "/", .. }));
        assert!(matches!(plan_complete("!l"), CompletePlan::Verbs { ns: "!", .. }));
        assert!(matches!(plan_complete("wh"), CompletePlan::Verbs { ns: "", .. }));
        // path completion after a space on a path-taking verb
        assert!(matches!(plan_complete("/mount foo"), CompletePlan::Path));
        assert!(matches!(plan_complete("!cd sub"), CompletePlan::Path));
        // a non-path verb followed by args → nothing
        assert!(matches!(plan_complete("/eject x"), CompletePlan::Nothing));
        assert!(matches!(plan_complete("!pwd x"), CompletePlan::Nothing));
        // a bare monitor command WITH an argument → nothing (was the old behaviour for
        // `d c000`); an empty line → nothing.
        assert!(matches!(plan_complete("d c000"), CompletePlan::Nothing));
        assert!(matches!(plan_complete(""), CompletePlan::Nothing));
    }

    #[test]
    fn plan_bare_monitor_verb_completes() {
        // A bare, space-free token completes against the curated monitor verb set.
        match plan_complete("who") {
            CompletePlan::Verbs { ns, stem, set } => {
                assert_eq!(ns, "");
                assert_eq!(stem, "who");
                let (line, listing) = complete_verb(ns, &stem, set);
                assert_eq!(line, "whowrote "); // unique monitor verb
                assert!(listing.is_empty());
            }
            _ => panic!("bare monitor verb should complete"),
        }
    }

    #[test]
    fn split_path_token_unquoted_last_token() {
        assert_eq!(
            split_path_token("!cd sub/fo"),
            PathTok { head: "!cd ".into(), partial: "sub/fo".into(), quoted: false }
        );
    }

    #[test]
    fn split_path_token_quoted_allows_spaces() {
        // an open double-quote captures the rest as one path, spaces and all
        assert_eq!(
            split_path_token("!load \"my ga"),
            PathTok { head: "!load ".into(), partial: "my ga".into(), quoted: true }
        );
    }

    #[test]
    fn split_path_token_no_space_is_whole_input() {
        assert_eq!(
            split_path_token("abc"),
            PathTok { head: String::new(), partial: "abc".into(), quoted: false }
        );
    }

    #[test]
    fn apply_single_file_fills_and_spaces() {
        let tok = PathTok { head: "!load ".into(), partial: "loa".into(), quoted: false };
        assert_eq!(apply_path_completion(&tok, "loader.prg", Some(false)), "!load loader.prg ");
    }

    #[test]
    fn apply_single_dir_appends_slash() {
        let tok = PathTok { head: "!cd ".into(), partial: "su".into(), quoted: false };
        assert_eq!(apply_path_completion(&tok, "sub", Some(true)), "!cd sub/");
    }

    #[test]
    fn apply_many_fills_common_prefix_only() {
        let tok = PathTok { head: "!ls ".into(), partial: "a".into(), quoted: false };
        assert_eq!(apply_path_completion(&tok, "a", None), "!ls a");
    }

    #[test]
    fn apply_requotes_when_completed_has_space() {
        // an unquoted token whose completion contains a space → wrap it in quotes
        let tok = PathTok { head: "!load ".into(), partial: "my".into(), quoted: false };
        assert_eq!(
            apply_path_completion(&tok, "my game.prg", Some(false)),
            "!load \"my game.prg\" "
        );
    }

    #[test]
    fn apply_preserves_quote_and_reattaches_dir_part() {
        // quoted token with a dir part; the daemon `common` is relative to the dir part,
        // so it must be re-attached, and the quote preserved + closed on a file.
        let tok = PathTok { head: "!load ".into(), partial: "sub/lo".into(), quoted: true };
        assert_eq!(
            apply_path_completion(&tok, "loader.prg", Some(false)),
            "!load \"sub/loader.prg\" "
        );
    }

    #[test]
    fn candidate_log_lines_color_by_filetype() {
        let entries = vec![
            json!({ "name": "sub", "is_dir": true }),
            json!({ "name": "a.crt", "is_dir": false }),
        ];
        let lines = candidate_log_lines(&entries);
        assert_eq!(lines.len(), 2);
        // a dir gets a trailing '/' + dir style
        assert_eq!(lines[0].text, "  sub/");
        assert_eq!(lines[0].style, Some(crate::ftcolor::style_for("sub", true)));
        // a file is colored by its extension
        assert_eq!(lines[1].text, "  a.crt");
        assert_eq!(lines[1].style, Some(crate::ftcolor::style_for("a.crt", false)));
    }

    /// rpc-backed: boot a machine (skip when ROMs absent), point the FS cwd at a temp
    /// dir, and exercise `complete_path` + the `autocomplete` orchestrator end-to-end.
    #[test]
    fn path_complete_against_live_cwd() {
        let rom_dir = crate::default_rom_dir();
        if !rom_dir.join("kernal-901227-03.bin").exists() {
            eprintln!("[skip] path_complete_against_live_cwd: ROMs absent at {}", rom_dir.display());
            return;
        }
        let engine = match crate::boot_engine(&rom_dir) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[skip] path_complete_against_live_cwd: boot failed: {e}");
                return;
            }
        };
        let dir = std::env::temp_dir().join(format!("trx64_s5_pathcomp_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.crt"), b"").unwrap();
        std::fs::write(dir.join("a2.crt"), b"").unwrap();
        std::fs::write(dir.join("uniquefile.prg"), b"").unwrap();
        std::fs::create_dir_all(dir.join("subby")).unwrap();

        engine.exec_line(&format!("!cd {}", dir.display()));

        // Ambiguous: "!ls a" → common "a" (no growth), colored list of both .crt files.
        let (line, listing) = complete_path("!ls a", &engine);
        assert_eq!(line, "!ls a");
        let names: Vec<&str> = listing.iter().map(|l| l.text.trim()).collect();
        assert!(names.contains(&"a.crt") && names.contains(&"a2.crt"), "names: {names:?}");
        // the candidate list is COLORED (.crt → yellow via ftcolor)
        assert!(listing
            .iter()
            .any(|l| l.style == Some(crate::ftcolor::style_for("a.crt", false))));

        // Unique file: "!load uni" → fill + trailing space, no listing.
        let (line, listing) = complete_path("!load uni", &engine);
        assert_eq!(line, "!load uniquefile.prg ");
        assert!(listing.is_empty());

        // Unique dir: "!cd sub" → fill + trailing slash.
        let (line, _) = complete_path("!cd sub", &engine);
        assert_eq!(line, "!cd subby/");

        // Orchestrator wiring: a verb completion (no rpc) sets the line + parks cursor.
        let mut cp = Cockpit::new();
        cp.set_line("/pow".into());
        autocomplete(&mut cp, &engine);
        assert_eq!(cp.input, "/power ");
        assert_eq!(cp.cursor, cp.line_char_len());

        // Orchestrator wiring: a path completion (rpc-backed) fills the line.
        let mut cp = Cockpit::new();
        cp.set_line("!cd sub".into());
        autocomplete(&mut cp, &engine);
        assert_eq!(cp.input, "!cd subby/");
        assert_eq!(cp.cursor, cp.line_char_len());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── S4: !ls filetype coloring ─────────────────────────────────────────────

    #[test]
    fn ls_output_is_filetype_colored_by_flag_column() {
        // The daemon ls verb format: "{dir}:" header + "  {d|-} {name}" per entry.
        let output = "/games:\n  d subdir\n  - game.crt\n  - disk.d64\n  - loader.prg\n  - notes.md\n  - readme";
        let styled = ls_styled_lines("!ls", output).expect("!ls output is colored");
        // header stays plain
        assert_eq!(styled[0].text, "/games:");
        assert_eq!(styled[0].style, None);
        // dir-ness comes from the `d|-` column, not the name's extension
        assert_eq!(styled[1].style, Some(crate::ftcolor::style_for("subdir", true)));
        assert_eq!(styled[2].style, Some(crate::ftcolor::style_for("game.crt", false)));
        assert_eq!(styled[3].style, Some(crate::ftcolor::style_for("disk.d64", false)));
        assert_eq!(styled[4].style, Some(crate::ftcolor::style_for("loader.prg", false)));
        assert_eq!(styled[5].style, Some(crate::ftcolor::style_for("notes.md", false)));
        // no-extension file → default (Other) style, still Some (colored path taken)
        assert_eq!(styled[6].style, Some(crate::ftcolor::style_for("readme", false)));
        // the displayed text is preserved verbatim (flag column + name)
        assert_eq!(styled[2].text, "  - game.crt");
    }

    #[test]
    fn ls_header_and_empty_sentinel_stay_plain() {
        let styled = ls_styled_lines("!ls", "/empty:\n  (empty)").expect("colored");
        assert_eq!(styled[0].style, None); // "{dir}:" header
        assert_eq!(styled[1].style, None); // "  (empty)" sentinel
    }

    #[test]
    fn ls_dir_alias_and_arg_are_colored() {
        // `!dir` alias + an explicit path arg both take the coloring path.
        let styled = ls_styled_lines("!dir sub", "/root/sub:\n  - a.d64").expect("!dir colored");
        assert_eq!(styled[1].style, Some(crate::ftcolor::style_for("a.d64", false)));
    }

    #[test]
    fn non_ls_commands_are_not_colored() {
        // bare monitor command
        assert_eq!(ls_styled_lines("d c000", "c000: ..."), None);
        // another FS verb behind `!`
        assert_eq!(ls_styled_lines("!pwd", "/home"), None);
        // VM command
        assert_eq!(ls_styled_lines("/mount foo.crt", "mounted"), None);
        // a bare `ls` is a cockpit nudge, not the `!` routing layer → not colored here
        assert_eq!(ls_styled_lines("ls", "  - x.crt"), None);
    }

    #[test]
    fn ls_entry_style_rejects_malformed_lines() {
        assert_eq!(ls_entry_style("no-indent"), None); // missing 2-space indent
        assert_eq!(ls_entry_style("  x foo"), None); // flag col isn't d/-
        assert_eq!(ls_entry_style("  d"), None); // no separator space after the flag
        assert_eq!(ls_entry_style("  (empty)"), None); // the empty sentinel
        assert_eq!(ls_entry_style("  - a.crt"), Some(crate::ftcolor::style_for("a.crt", false)));
        assert_eq!(ls_entry_style("  d sub"), Some(crate::ftcolor::style_for("sub", true)));
    }

    #[test]
    fn push_log_styled_appends_and_snaps_to_tail() {
        let mut cp = Cockpit::new();
        cp.scroll = 5;
        let before = cp.log.len();
        cp.push_log_styled(vec![LogLine {
            text: "  - x.crt".into(),
            style: Some(crate::ftcolor::style_for("x.crt", false)),
            spans: None,
        }]);
        assert_eq!(cp.log.len(), before + 1);
        assert_eq!(cp.log.last().unwrap().style, Some(crate::ftcolor::style_for("x.crt", false)));
        assert_eq!(cp.scroll, 0); // new output snaps back to the live tail
    }

    // ── S6: readline kill/word muscles + persistent, deduped history ────────────

    #[test]
    fn kill_to_end_drops_from_cursor() {
        let mut cp = Cockpit::new();
        cp.set_line("hello world".into());
        cp.cursor = 5; // "hello| world"
        cp.kill_to_end();
        assert_eq!(cp.input, "hello");
        assert_eq!(cp.cursor, 5);
        // at the end it is a no-op, not a panic
        cp.kill_to_end();
        assert_eq!(cp.input, "hello");
    }

    #[test]
    fn kill_to_start_drops_before_cursor() {
        let mut cp = Cockpit::new();
        cp.set_line("hello world".into());
        cp.cursor = 6; // "hello |world"
        cp.kill_to_start();
        assert_eq!(cp.input, "world");
        assert_eq!(cp.cursor, 0);
        // at the start it is a no-op
        cp.kill_to_start();
        assert_eq!(cp.input, "world");
    }

    #[test]
    fn delete_word_before_eats_word_and_trailing_space() {
        let mut cp = Cockpit::new();
        cp.set_line("/mount some.crt".into());
        cp.cursor = cp.line_char_len(); // end
        cp.delete_word_before();
        assert_eq!(cp.input, "/mount ");
        assert_eq!(cp.cursor, 7);
        // a second Ctrl-W eats the trailing space + the "/mount" word
        cp.delete_word_before();
        assert_eq!(cp.input, "");
        assert_eq!(cp.cursor, 0);
        // no-op at column 0
        cp.delete_word_before();
        assert_eq!(cp.input, "");
    }

    #[test]
    fn delete_word_before_midline_keeps_tail() {
        let mut cp = Cockpit::new();
        cp.set_line("abc def ghi".into());
        cp.cursor = 7; // "abc def| ghi" → delete "def"
        cp.delete_word_before();
        assert_eq!(cp.input, "abc  ghi");
        assert_eq!(cp.cursor, 4);
    }

    #[test]
    fn kill_ops_are_char_safe() {
        // multi-byte chars must not split a codepoint / panic
        let mut cp = Cockpit::new();
        cp.set_line("héllo wörld".into());
        cp.cursor = 6; // after "héllo " (6 chars)
        cp.kill_to_start();
        assert_eq!(cp.input, "wörld");
        assert_eq!(cp.cursor, 0);
    }

    #[test]
    fn push_history_dedups_consecutive() {
        let mut cp = Cockpit::new();
        assert!(cp.push_history("d c000"));
        assert!(!cp.push_history("d c000")); // consecutive dup → skipped
        assert!(cp.push_history("g"));
        assert!(cp.push_history("d c000")); // non-consecutive → kept again
        assert_eq!(cp.history, vec!["d c000", "g", "d c000"]);
    }

    #[test]
    fn push_history_caps_at_hist_cap() {
        let mut cp = Cockpit::new();
        for i in 0..(HIST_CAP + 50) {
            assert!(cp.push_history(&format!("cmd{i}")));
        }
        assert_eq!(cp.history.len(), HIST_CAP);
        // the oldest entries were dropped; the newest survives
        assert_eq!(cp.history.first().unwrap(), "cmd50");
        assert_eq!(cp.history.last().unwrap(), &format!("cmd{}", HIST_CAP + 49));
    }

    #[test]
    fn history_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("trx64_s6_hist_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(".trx64").join("history");

        // append several entries (dir is created on first write)
        append_history_line(&path, "d c000");
        append_history_line(&path, "g");
        append_history_line(&path, "!ls");

        let loaded = load_history_from(&path);
        assert_eq!(loaded, vec!["d c000", "g", "!ls"]);

        // a missing file loads as empty, not an error
        assert!(load_history_from(&dir.join("nope")).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_history_caps_to_the_tail() {
        let dir = std::env::temp_dir().join(format!("trx64_s6_histcap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history");
        let body: String =
            (0..(HIST_CAP + 25)).map(|i| format!("cmd{i}\n")).collect();
        std::fs::write(&path, body).unwrap();

        let loaded = load_history_from(&path);
        assert_eq!(loaded.len(), HIST_CAP);
        assert_eq!(loaded.first().unwrap(), "cmd25"); // oldest tail-trimmed
        assert_eq!(loaded.last().unwrap(), &format!("cmd{}", HIST_CAP + 24));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_history_file_stays_bounded() {
        // The persisted file (not just the in-memory ring / load path) must stay capped:
        // appending far past the cap compacts it back down instead of growing forever.
        let dir = std::env::temp_dir().join(format!("trx64_s6_histbound_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(".trx64").join("history");

        let total = HIST_CAP + HIST_FILE_SLACK + 500;
        for i in 0..total {
            append_history_line(&path, &format!("cmd{i}"));
        }

        // On disk: never more than cap + slack lines, and at least the full cap survives.
        let on_disk: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        assert!(
            on_disk.len() <= HIST_CAP + HIST_FILE_SLACK,
            "history file grew unbounded: {} lines",
            on_disk.len()
        );
        assert!(on_disk.len() >= HIST_CAP, "history file over-trimmed: {} lines", on_disk.len());

        // The newest entry is always retained; the oldest have been dropped.
        assert_eq!(on_disk.last().unwrap(), &format!("cmd{}", total - 1));
        assert!(!on_disk.iter().any(|l| l == "cmd0"), "oldest entry should be trimmed");

        // And the loader still yields exactly the last HIST_CAP entries.
        let loaded = load_history_from(&path);
        assert_eq!(loaded.len(), HIST_CAP);
        assert_eq!(loaded.last().unwrap(), &format!("cmd{}", total - 1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_cockpit_has_no_history_side_effects() {
        // new() must stay pure: no disk read, no hist_path (loaded only in run_loop).
        let cp = Cockpit::new();
        assert!(cp.history.is_empty());
        assert!(cp.hist_path.is_none());
    }

    // ── S10: structured, colored, width-aware INLINE help rendering ─────────────

    /// The styles the formatter uses, mirrored here so assertions read clearly.
    fn help_section_style() -> Style {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    }
    fn help_verb_style() -> Style {
        Style::default().fg(Color::Green)
    }
    fn help_sub_style() -> Style {
        Style::default().fg(Color::DarkGray)
    }

    #[test]
    fn help_command_trigger_is_case_insensitive() {
        assert!(is_help_command("help"));
        assert!(is_help_command("HELP"));
        assert!(is_help_command("?"));
        assert!(is_help_command("/help"));
        assert!(is_help_command("!help"));
        assert!(is_help_command("  Help  ")); // trimmed + lowercased like exec_line
        // not help — must fall through to the normal output path
        assert!(!is_help_command("helpme"));
        assert!(!is_help_command("d c000"));
        assert!(!is_help_command("/helpx"));
    }

    #[test]
    fn classify_detects_each_line_shape() {
        // The aligned description column of the real monitor help. Indent-based shapes
        // (blank/title/section/sub) return before it matters; verb/spec shapes below use it.
        let col = Some(17);
        assert_eq!(classify_help_line("", col), HelpLine::Blank);
        assert_eq!(classify_help_line("      ", col), HelpLine::Blank);
        assert_eq!(
            classify_help_line("monitor (VICE-superset):", col),
            HelpLine::Title("monitor (VICE-superset):")
        );
        // 2-space, no verb/desc split → section header
        assert_eq!(classify_help_line("  EXEC", col), HelpLine::Section("EXEC"));
        assert_eq!(
            classify_help_line("  MEMORY (bank lens: cpu|ram)", col),
            HelpLine::Section("MEMORY (bank lens: cpu|ram)")
        );
        // 4-space verb row split on the FIRST run of >=2 spaces
        assert_eq!(
            classify_help_line("    g [addr]         go/resume the run-loop", col),
            HelpLine::Verb { verb: "g [addr]", desc: "go/resume the run-loop" }
        );
        // S10 regression: column-aligned verb rows whose verb+args fills the column, so only
        // a SINGLE space separates verb from description — the three most-used monitor
        // commands. These must classify as Verb (green verb + description), NOT a fully-green
        // Spec. `m`/`bk` sit exactly at the column; `d`'s args overflow it by 2 chars.
        assert_eq!(
            classify_help_line(
                "    m [lens] <a> [b] memory dump ($20/row + petscii; default len $800)",
                col
            ),
            HelpLine::Verb {
                verb: "m [lens] <a> [b]",
                desc: "memory dump ($20/row + petscii; default len $800)"
            }
        );
        assert_eq!(
            classify_help_line(
                "    d [lens] [a] [end] disassemble: a..end range (VICE), or ~16 from a/PC",
                col
            ),
            HelpLine::Verb {
                verb: "d [lens] [a] [end]",
                desc: "disassemble: a..end range (VICE), or ~16 from a/PC"
            }
        );
        assert_eq!(
            classify_help_line("    bk <a> | bk -<a> set / remove breakpoint (by addr)", col),
            HelpLine::Verb { verb: "bk <a> | bk -<a>", desc: "set / remove breakpoint (by addr)" }
        );
        // 4-space, all single spaces, NO description → a no-desc spec. These must STAY Spec
        // even with the column known: their next space is nowhere near the column.
        assert_eq!(
            classify_help_line("    obs <name> when exec <a> do <action>", col),
            HelpLine::Spec("obs <name> when exec <a> do <action>")
        );
        assert_eq!(
            classify_help_line(
                "    obs <name> when exec|load|store <a[..b]> [if <cond>] do <action> [fields]",
                col
            ),
            HelpLine::Spec(
                "obs <name> when exec|load|store <a[..b]> [if <cond>] do <action> [fields]"
            )
        );
        assert_eq!(
            classify_help_line("    ignore <name> [n]", col),
            HelpLine::Spec("ignore <name> [n]")
        );
        // Without a detected column (no verb rows to align to) the single-space fallback is
        // disabled: the same `m` row can't be split and stays a Spec.
        assert_eq!(
            classify_help_line("    m [lens] <a> [b] memory dump", None),
            HelpLine::Spec("m [lens] <a> [b] memory dump")
        );
        // 6+-space indent → sub/continuation, EVEN with an inner 2-space run
        assert_eq!(
            classify_help_line("      log fields: a/x/y  e.g. `do log $fd`", col),
            HelpLine::Sub("log fields: a/x/y  e.g. `do log $fd`")
        );
        assert_eq!(
            classify_help_line("                     `swimlane <s> <e>` deep continuation", col),
            HelpLine::Sub("`swimlane <s> <e>` deep continuation")
        );
    }

    #[test]
    fn detect_help_desc_col_finds_the_aligned_column() {
        // Mixed widths, but the descriptions align at content-column 17 (indent 4 + a
        // 13-wide padded verb column). The mode wins over the wider outliers.
        let raw = "\
monitor (VICE-superset):
  EXEC
    g [addr]         go/resume the run-loop
    x                exit/resume (= g)
    until <addr>     run until PC=addr, then stop
    swimlane [list|name] [s] [e]  trace lanes (wide verb, later column)";
        assert_eq!(detect_help_desc_col(raw), Some(17));
        // No indent 3..=5 verb rows at all → nothing to align to.
        assert_eq!(detect_help_desc_col("title:\n  SECTION\n"), None);
    }

    #[test]
    fn section_header_renders_cyan_bold() {
        let out = format_help_lines("  EXEC", 80);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "  EXEC"); // indent preserved
        assert_eq!(out[0].style, Some(help_section_style()));
    }

    #[test]
    fn verb_row_splits_and_greens_the_verb() {
        let out = format_help_lines("    g [addr]         go/resume the run-loop", 80);
        assert_eq!(out.len(), 1);
        let spans = out[0].spans.as_ref().expect("verb row is multi-span");
        // exactly one span carries the verb text, and it is green
        let verb_span = spans.iter().find(|(t, _)| t == "g [addr]").expect("green verb span");
        assert_eq!(verb_span.1, help_verb_style());
        // the description span is default-fg (not green, not cyan)
        assert!(spans.iter().any(|(t, s)| t.contains("go/resume") && *s == Style::default()));
        // the visible text keeps the verb + aligned column + description
        assert!(out[0].text.starts_with("    g [addr]"));
        assert!(out[0].text.contains("go/resume the run-loop"));
    }

    #[test]
    fn long_description_hangs_under_the_desc_column() {
        // one section + one verb row whose description must wrap several times at width 40
        let raw = "  EXEC\n    g [addr]         resume the run-loop from the given address and \
                   keep going until the pause button halts it again for good";
        let width = 40;
        let out = format_help_lines(raw, width);

        // No emitted line exceeds the width (char count, not bytes).
        for l in &out {
            assert!(
                l.text.chars().count() <= width,
                "line exceeds width {width}: {:?} ({} chars)",
                l.text,
                l.text.chars().count()
            );
        }

        // verb_col = len("g [addr]") = 8 → desc column = 4 + 8 + 2 = 14.
        let desc_col = 14;
        // out[0] = section header; out[1] = the inline verb+first-desc line; the rest are
        // wrapped continuation lines, each indented to the description column.
        assert_eq!(out[0].text, "  EXEC");
        assert!(out[1].spans.is_some(), "first verb line is multi-span");
        assert!(out.len() > 2, "the long description should wrap onto continuation lines");
        for cont in &out[2..] {
            assert!(
                cont.text.starts_with(&" ".repeat(desc_col)),
                "continuation not aligned to desc col {desc_col}: {:?}",
                cont.text
            );
            // and the continuation carries actual description text past the indent
            assert!(!cont.text.trim().is_empty());
            assert_eq!(cont.style, Some(Style::default()));
        }
    }

    #[test]
    fn long_verb_goes_on_its_own_line_then_desc_underneath() {
        // A verb far wider than the ~18 cap: emitted on its own green line(s), then the
        // description indented beneath it.
        let raw = "    bitmap <a> [w h] [hires|charset|sprite]  render a RAM range to a PNG";
        let width = 60;
        let out = format_help_lines(raw, width);
        assert!(out.len() >= 2);
        // the first line is the green verb/spec (single-style, not the inline multi-span)
        assert!(out[0].spans.is_none());
        assert_eq!(out[0].style, Some(help_verb_style()));
        assert!(out[0].text.trim_start().starts_with("bitmap <a>"));
        // a later line carries the description, default-fg
        assert!(out
            .iter()
            .any(|l| l.text.contains("render a RAM range") && l.style == Some(Style::default())));
        for l in &out {
            assert!(l.text.chars().count() <= width);
        }
    }

    #[test]
    fn no_desc_spec_wraps_green_without_panic() {
        // the `obs …` spec is a single long no-desc row — must wrap (green) at a narrow
        // width without panicking, and every line must fit.
        let raw =
            "    obs <name> when exec|load|store <a[..b]> [if <cond>] do <action> [fields]";
        let width = 24;
        let out = format_help_lines(raw, width);
        assert!(!out.is_empty());
        for l in &out {
            assert_eq!(l.style, Some(help_verb_style())); // whole spec is green
            assert!(l.text.chars().count() <= width);
        }
    }

    #[test]
    fn sub_lines_render_dark_gray() {
        let out = format_help_lines("      actions: break | log | mark | trace", 80);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].style, Some(help_sub_style()));
        assert!(out[0].text.starts_with("      ")); // hangs at the sub indent
    }

    #[test]
    fn wrap_is_utf8_char_safe() {
        // a description with multi-byte codepoints, wrapped narrow: no codepoint is split
        // (every produced line is valid UTF-8 by construction, and the char total is
        // preserved modulo the collapsed inter-word spaces).
        let text = "café ünïcödé — schöne grüße naïve résumé façade";
        let segs = wrap_help_text(text, 7);
        for s in &segs {
            assert!(s.chars().count() <= 7, "seg too wide: {s:?}");
        }
        // the multi-byte word survives intact somewhere across the wrapped lines
        let joined = segs.join(" ");
        assert!(joined.contains("ünïcödé"));
        assert!(joined.contains("grüße"));

        // a single word LONGER than the width is hard-split on char boundaries (no panic)
        let one_long = wrap_help_text("ünïcödé-très-lööng", 4);
        assert!(one_long.iter().all(|s| s.chars().count() <= 4));
        assert_eq!(one_long.concat().replace(' ', ""), "ünïcödé-très-lööng");
    }

    #[test]
    fn tiny_width_does_not_panic() {
        // width 0 / 1 must not panic (clamped to a usable minimum) and must yield output.
        for w in [0usize, 1, 2, 3] {
            let out = format_help_lines(
                "monitor (VICE-superset):\n  EXEC\n    g [addr]         go/resume\n      sub line",
                w,
            );
            assert!(!out.is_empty(), "width {w} produced no lines");
        }
    }

    #[test]
    fn real_monitor_help_slice_formats_without_panic() {
        // A verbatim slice of monitor_help_text() (main.rs) — the mix of title, section,
        // aligned verbs, single-space verbs, no-desc specs, and deep continuations must
        // all format cleanly.
        let raw = "\
monitor (VICE-superset):
  EXEC
    g [addr]         go/resume the run-loop (PC=addr); Pause button halts
    x                exit/resume (= g)
    until <addr>     run until PC=addr, then stop (synchronous)
    reset            cold reset
  MEMORY (bank lens: cpu|ram|rom|io|cart, default cpu = what CPU sees)
    m [lens] <a> [b] memory dump ($20/row + petscii; default len $800)
    d [lens] [a] [end] disassemble: a..end range (VICE), or ~16 from a/PC
  BREAKPOINTS / OBSERVERS
    bk               list breakpoints (#num $addr)
    bk <a> | bk -<a> set / remove breakpoint (by addr)
    obs <name> when exec|load|store <a[..b]> [if <cond>] do <action> [fields]
      actions: break | log [fields] | mark [\"label\"] | cmd \"<cmd>\" | trace [domains]|off
    swimlane [list|name] [s] [e]  trace lanes (cpu/irq/nmi/io/1541): list / newest
                     `swimlane <s> <e>` with no covering trace → auto checkpoint-ring replay";
        for width in [80usize, 60, 40, 20, 10, 1] {
            let out = format_help_lines(raw, width);
            assert!(!out.is_empty(), "width {width} produced no lines");
            let eff = width.max(HELP_MIN_WIDTH);
            for l in &out {
                assert!(
                    l.text.chars().count() <= eff,
                    "width {width}: line exceeds {eff}: {:?}",
                    l.text
                );
            }
        }

        // S10 regression (positive render check at a comfortable width): the single-space
        // rows `m`/`d`/`bk` must render as a green verb span + a default-fg description span,
        // NOT as a fully-green Spec — and their descriptions must align to the SAME column as
        // an ordinary verb row (`g [addr]`).
        let out = format_help_lines(raw, 80);
        // The rendered description column for a `verb` row: char index of `needle` in the
        // (single) LogLine that carries a green span exactly equal to `verb`.
        let desc_col_of = |verb: &str, needle: &str| -> Option<usize> {
            out.iter().find_map(|l| {
                let spans = l.spans.as_ref()?;
                let is_verb_row = spans.iter().any(|(t, s)| t == verb && *s == help_verb_style());
                if !is_verb_row {
                    return None;
                }
                // the verb is green and some later span carries the (default-fg) description
                assert!(
                    spans
                        .iter()
                        .any(|(t, s)| t.contains(needle) && *s == Style::default()),
                    "row for verb {verb:?} has no default-fg description span containing {needle:?}: {:?}",
                    l.text
                );
                l.text.find(needle).map(|b| l.text[..b].chars().count())
            })
        };

        let g_col = desc_col_of("g [addr]", "go/resume").expect("g [addr] verb row");
        let m_col = desc_col_of("m [lens] <a> [b]", "memory dump").expect("m verb row (single-space)");
        let d_col = desc_col_of("d [lens] [a] [end]", "disassemble:").expect("d verb row (single-space)");
        let bk_col =
            desc_col_of("bk <a> | bk -<a>", "set / remove").expect("bk verb row (single-space)");
        assert_eq!(m_col, g_col, "m description must align to the verb-row desc column");
        assert_eq!(d_col, g_col, "d description must align to the verb-row desc column");
        assert_eq!(bk_col, g_col, "bk description must align to the verb-row desc column");
    }
}
