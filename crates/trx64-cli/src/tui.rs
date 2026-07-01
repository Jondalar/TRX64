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

use std::io::{self, Stdout};
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

/// One line in the OUTPUT/LOG pane. `style`, when set, overrides the draw-time content
/// heuristic (banner / `> ` echo) — used by `!ls` filetype coloring (S4) and, later,
/// the colored Tab candidate lists (S5). Plain lines carry `style: None` and keep the
/// existing content-based styling in [`draw_log`].
#[derive(Debug, Clone, PartialEq)]
struct LogLine {
    text: String,
    style: Option<Style>,
}

impl From<&str> for LogLine {
    fn from(s: &str) -> Self {
        LogLine { text: s.to_string(), style: None }
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
    snap: StateSnapshot,
    /// Lines scrolled UP from the bottom of the log (0 = pinned to the tail). Mouse
    /// wheel adjusts it; any new output snaps back to 0.
    scroll: usize,
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
            snap: StateSnapshot::default(),
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

fn run_loop(term: &mut Term, engine: &Engine, to_main: &Sender<UiToMain>) -> io::Result<()> {
    let mut cp = Cockpit::new();
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
                    // Ctrl-C / Ctrl-D quit the cockpit.
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, XKeyCode::Char('c') | XKeyCode::Char('d'))
                    {
                        let _ = engine.exec_line("/quit");
                        let _ = to_main.send(UiToMain::Quit);
                        return Ok(());
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
                        }
                        XKeyCode::Delete => {
                            cp.delete_at();
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
                            cp.history.push(line.clone());
                            cp.push_log(&format!("> {line}"));
                            let r = engine.exec_line(&line);
                            if !r.output.is_empty() {
                                // S4: `!ls`/`!dir` output is filetype-colored per entry;
                                // every other command's output logs verbatim.
                                match ls_styled_lines(&line, &r.output) {
                                    Some(styled) => cp.push_log_styled(styled),
                                    None => cp.push_log(&r.output),
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
            LogLine { text, style: Some(crate::ftcolor::style_for(name, is_dir)) }
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
            .map(|l| LogLine { text: l.to_string(), style: ls_entry_style(l) })
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
    // Scroll: `cp.scroll` lines up from the tail, clamped so the window stays in range.
    let max_scroll = cp.log.len().saturating_sub(inner_h);
    let scroll = cp.scroll.min(max_scroll);
    let end = cp.log.len() - scroll;
    let start = end.saturating_sub(inner_h);
    let lines: Vec<Line> = cp.log[start..end]
        .iter()
        .map(|l| {
            if let Some(style) = l.style {
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
        }]);
        assert_eq!(cp.log.len(), before + 1);
        assert_eq!(cp.log.last().unwrap().style, Some(crate::ftcolor::style_for("x.crt", false)));
        assert_eq!(cp.scroll, 0); // new output snaps back to the live tail
    }
}
