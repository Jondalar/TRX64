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
                        // Tab: complete the /-command verb.
                        XKeyCode::Tab => {
                            if let Some(msg) = autocomplete(&mut cp.input) {
                                cp.push_log(&msg);
                            }
                            cp.cursor = cp.line_char_len();
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

/// Tab-complete a /-command verb in place. Returns a message listing candidates when
/// the prefix is ambiguous (input is also completed to the longest common prefix); on a
/// single match the verb is filled in with a trailing space; `None` when nothing to do.
fn autocomplete(input: &mut String) -> Option<String> {
    const VERBS: [&str; 17] = [
        "power", "reset", "run", "pause", "step", "mount", "eject", "load", "warp", "joystick",
        "window", "dump", "restore", "ringdump", "ringload", "help", "quit",
    ];
    let rest = input.strip_prefix('/')?;
    if rest.contains(' ') {
        return None; // verb already entered — args aren't completed
    }
    let matches: Vec<&str> = VERBS.iter().copied().filter(|v| v.starts_with(rest)).collect();
    match matches.as_slice() {
        [] => None,
        [only] => {
            *input = format!("/{only} ");
            None
        }
        many => {
            let lcp = longest_common_prefix(many);
            if lcp.len() > rest.len() {
                *input = format!("/{lcp}");
            }
            Some(format!(
                "  {}",
                many.iter().map(|m| format!("/{m}")).collect::<Vec<_>>().join("  ")
            ))
        }
    }
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

    #[test]
    fn autocomplete_single_completes_with_space() {
        let mut s = "/pow".to_string();
        assert_eq!(autocomplete(&mut s), None);
        assert_eq!(s, "/power ");
    }

    #[test]
    fn autocomplete_ambiguous_fills_common_prefix_and_lists() {
        // /re → {reset, restore}; LCP = "res".
        let mut s = "/re".to_string();
        let msg = autocomplete(&mut s).expect("candidates listed");
        assert_eq!(s, "/res");
        assert!(msg.contains("/reset") && msg.contains("/restore"));
    }

    #[test]
    fn autocomplete_ignores_bare_monitor_lines() {
        let mut s = "d c000".to_string();
        assert_eq!(autocomplete(&mut s), None);
        assert_eq!(s, "d c000");
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
