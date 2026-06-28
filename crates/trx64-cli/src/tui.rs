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

use crossterm::event::{self, Event, KeyCode as XKeyCode, KeyEventKind, KeyModifiers};
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
    execute!(stdout, terminal::EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(term: &mut Term) -> io::Result<()> {
    terminal::disable_raw_mode()?;
    execute!(term.backend_mut(), terminal::LeaveAlternateScreen)?;
    term.show_cursor()
}

struct Cockpit {
    input: String,
    log: Vec<String>,
    history: Vec<String>,
    hist_idx: Option<usize>,
    snap: StateSnapshot,
}

impl Cockpit {
    fn new() -> Self {
        Self {
            input: String::new(),
            log: vec![
                "TRX64 cockpit — powered on + running. A bare line goes to the monitor".into(),
                "(d / m / r / bk / g / trace …); /-commands drive the machine.".into(),
                "/help · /window spawns the emulator · /pause freezes · /quit exits.".into(),
                String::new(),
            ],
            history: Vec::new(),
            hist_idx: None,
            snap: StateSnapshot::default(),
        }
    }

    fn push_log(&mut self, text: &str) {
        for line in text.split('\n') {
            self.log.push(line.to_string());
        }
        // Bound the log so it doesn't grow unbounded over a long session.
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
            if let Event::Key(key) = event::read()? {
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
                    XKeyCode::Char(c) => {
                        cp.input.push(c);
                        cp.hist_idx = None;
                    }
                    XKeyCode::Backspace => {
                        cp.input.pop();
                    }
                    XKeyCode::Up => {
                        if !cp.history.is_empty() {
                            let i = match cp.hist_idx {
                                None => cp.history.len() - 1,
                                Some(0) => 0,
                                Some(i) => i - 1,
                            };
                            cp.hist_idx = Some(i);
                            cp.input = cp.history[i].clone();
                        }
                    }
                    XKeyCode::Down => {
                        if let Some(i) = cp.hist_idx {
                            if i + 1 < cp.history.len() {
                                cp.hist_idx = Some(i + 1);
                                cp.input = cp.history[i + 1].clone();
                            } else {
                                cp.hist_idx = None;
                                cp.input.clear();
                            }
                        }
                    }
                    XKeyCode::Enter => {
                        let line = cp.input.trim().to_string();
                        cp.input.clear();
                        cp.hist_idx = None;
                        if line.is_empty() {
                            continue;
                        }
                        cp.history.push(line.clone());
                        cp.push_log(&format!("> {line}"));
                        let r = engine.exec_line(&line);
                        if !r.output.is_empty() {
                            cp.push_log(&r.output);
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
        }
    }
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
    // Show the tail of the log that fits.
    let inner_h = area.height.saturating_sub(2) as usize;
    let start = cp.log.len().saturating_sub(inner_h);
    let lines: Vec<Line> = cp.log[start..]
        .iter()
        .map(|l| {
            if l.starts_with("> ") {
                Line::from(Span::styled(l.clone(), Style::default().fg(Color::Green)))
            } else {
                Line::from(Span::raw(l.clone()))
            }
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" OUTPUT / LOG "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_input(f: &mut Frame, area: Rect, cp: &Cockpit) {
    let line = Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw(cp.input.clone()),
        Span::styled("█", Style::default().fg(Color::Green)),
    ]);
    f.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title(" command ")),
        area,
    );
}

fn panel<'a>(lines: Vec<Line<'a>>, title: &'a str) -> Paragraph<'a> {
    Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(format!(" {title} ")))
}
