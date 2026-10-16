//! The live view. Each server polls on its own task; the screen redraws when something
//! changed (at most once a second), on keys and on resize. The body is drawn into a
//! buffer as tall as it needs and only the slice that fits is shown, so nothing is cut off
//! silently: too many servers switch to the compact view, the rest scrolls.

use std::io::Stdout;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget};
use ratatui::Terminal;
use tokio::sync::Notify;

use crate::render::{self, Ctx};
use crate::state::{jitter, Shared, View};

pub const HELP: [(&str, &str); 9] = [
    ("q", "quit"),
    ("p", "show / hide the process table"),
    ("c", "compact (one line per server) / full view"),
    ("r", "retry failed servers now"),
    ("space", "pause / resume"),
    ("j k ↓ ↑", "scroll"),
    ("PgDn PgUp", "scroll a page"),
    ("g G", "top / bottom"),
    ("? h", "this help"),
];

pub struct Pollers {
    pub views: Vec<Shared>,
    pub kick: Arc<Notify>,
    pub paused: Arc<AtomicBool>,
}

/// Start one polling task per source on `rt`.
pub fn spawn(
    rt: &tokio::runtime::Runtime,
    sources: Vec<(crate::remote::Source, Shared)>,
    interval: Duration,
) -> Pollers {
    let kick = Arc::new(Notify::new());
    let paused = Arc::new(AtomicBool::new(false));
    let mut views = Vec::new();
    for (i, (mut src, view)) in sources.into_iter().enumerate() {
        views.push(view.clone());
        let (kick, paused) = (kick.clone(), paused.clone());
        rt.spawn(async move {
            if !src.is_demo() {
                tokio::time::sleep(Duration::from_millis(50 * i as u64)).await; // stagger the first connects
            }
            loop {
                let due = view.lock().unwrap().due(Instant::now());
                if due && !paused.load(Ordering::Relaxed) {
                    let t0 = Instant::now();
                    let out = src.poll().await;
                    view.lock().unwrap().apply(out, t0, jitter());
                }
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = kick.notified() => {}
                }
            }
        });
    }
    Pollers {
        views,
        kick,
        paused,
    }
}

pub struct App {
    pub interval: f64,
    pub procs: bool,
    pub idle_after: Duration,
    pub max_gpus: Option<usize>,
    pub compact: Option<bool>, // None: automatic
    showing_compact: bool,
    offset: u16,
    page: u16,
    help: bool,
}

pub enum Action {
    None,
    Quit,
    Suspend,
    Retry,
    TogglePause,
}

impl App {
    pub fn new(interval: f64, procs: bool, compact: Option<bool>) -> Self {
        App {
            interval,
            procs,
            idle_after: Duration::from_secs(30 * 60),
            max_gpus: None,
            compact,
            showing_compact: false,
            offset: 0,
            page: 10,
            help: false,
        }
    }

    pub fn key(&mut self, k: KeyEvent) -> Action {
        if k.kind == KeyEventKind::Release {
            return Action::None;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        if ctrl && k.code == KeyCode::Char('z') {
            return Action::Suspend;
        }
        if self.help {
            self.help = false; // any key closes the help, q still quits
            return if matches!(k.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
                Action::Quit
            } else {
                Action::None
            };
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => return Action::Quit,
            KeyCode::Char('p') => self.procs = !self.procs,
            KeyCode::Char('c') => {
                self.compact = Some(!self.showing_compact);
                self.offset = 0;
            }
            KeyCode::Char('r') => return Action::Retry,
            KeyCode::Char(' ') => return Action::TogglePause,
            KeyCode::Char('?') | KeyCode::Char('h') => self.help = true,
            KeyCode::Char('j') | KeyCode::Down => self.offset = self.offset.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => self.offset = self.offset.saturating_sub(1),
            KeyCode::PageDown => self.offset = self.offset.saturating_add(self.page),
            KeyCode::PageUp => self.offset = self.offset.saturating_sub(self.page),
            KeyCode::Char('g') | KeyCode::Home => self.offset = 0,
            KeyCode::Char('G') | KeyCode::End => self.offset = u16::MAX,
            _ => {}
        }
        Action::None
    }

    /// Draw one frame into `buf` (the whole terminal).
    pub fn draw(
        &mut self,
        views: &[View],
        paused: bool,
        me: &str,
        area: Rect,
        buf: &mut ratatui::buffer::Buffer,
    ) {
        let mut ctx = Ctx::new(true, self.interval, self.procs, me.to_string());
        ctx.idle_after = self.idle_after;
        ctx.max_gpus = self.max_gpus;
        let rows = area.height.saturating_sub(1).max(1);
        let mut body = None;
        if self.compact != Some(true) {
            let full = render::body(views, &ctx, area.width, false);
            if self.compact == Some(false) || full.area.height <= rows {
                body = Some(full);
            }
        }
        self.showing_compact = body.is_none();
        let body = body.unwrap_or_else(|| render::body(views, &ctx, area.width, true));
        self.page = rows.saturating_sub(1).max(1);
        let max_off = body.area.height.saturating_sub(rows);
        self.offset = self.offset.min(max_off);
        let below = body.area.height.saturating_sub(self.offset + rows);

        let mut head = render::header(views, self.interval, area.width < 120);
        if paused {
            head.spans.push(Span::styled(
                "  PAUSED",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        }
        let mut hint = String::new();
        if self.offset > 0 || below > 0 {
            if self.offset > 0 {
                hint.push_str(&format!("  ↑{}", self.offset));
            }
            if below > 0 {
                hint.push_str(&format!("  ↓{}", below));
            }
        }
        hint.push_str(&format!(
            "  q quit  p procs  c {}  ? help",
            if self.showing_compact {
                "full"
            } else {
                "compact"
            }
        ));
        head.spans
            .push(Span::styled(hint, Style::new().fg(Color::Indexed(244))));
        Paragraph::new(head).render(Rect::new(area.x, area.y, area.width, 1), buf);
        render::copy_rows(&body, self.offset, buf, area.y + 1, rows);

        if self.help {
            let lines: Vec<Line> = HELP
                .iter()
                .map(|(k, what)| {
                    Line::from(vec![
                        Span::styled(
                            format!(" {:<11}", k),
                            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(*what),
                    ])
                })
                .collect();
            let (w, h) = (
                area.width.min(56),
                (lines.len() as u16 + 2).min(area.height.saturating_sub(1)),
            );
            let r = Rect::new(area.x + 1, area.y + 1, w, h);
            Clear.render(r, buf);
            Paragraph::new(lines)
                .block(
                    Block::new()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(" keys ")
                        .border_style(Style::new().fg(Color::Cyan)),
                )
                .render(r, buf);
        }
    }
}

struct Screen {
    term: Terminal<CrosstermBackend<Stdout>>,
}

impl Screen {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        execute!(out, EnterAlternateScreen, cursor::Hide)?;
        Ok(Screen {
            term: Terminal::new(CrosstermBackend::new(out))?,
        })
    }

    fn leave(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(self.term.backend_mut(), LeaveAlternateScreen, cursor::Show);
    }

    fn resume(&mut self) -> Result<()> {
        terminal::enable_raw_mode()?;
        execute!(self.term.backend_mut(), EnterAlternateScreen, cursor::Hide)?;
        self.term.clear()?;
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.leave();
    }
}

pub fn run(rt: tokio::runtime::Runtime, pollers: Pollers, mut app: App, me: String) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        let stop = stop.clone();
        rt.spawn(async move {
            if let Ok(mut s) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                s.recv().await;
                stop.store(true, Ordering::Relaxed);
            }
        });
    }
    let mut screen = Screen::enter()?;
    // restore the terminal even if we panic
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, cursor::Show);
        default_hook(info);
    }));

    let (mut drawn, mut drawn_at, mut size) =
        (u64::MAX, Instant::now() - Duration::from_secs(60), (0, 0));
    while !stop.load(Ordering::Relaxed) {
        let mut forced = false;
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(k) => {
                    forced = true;
                    match app.key(k) {
                        Action::Quit => break,
                        Action::Retry => {
                            for v in &pollers.views {
                                v.lock().unwrap().retry();
                            }
                            pollers.kick.notify_waiters();
                        }
                        Action::TogglePause => {
                            let p = !pollers.paused.load(Ordering::Relaxed);
                            pollers.paused.store(p, Ordering::Relaxed);
                        }
                        Action::Suspend => {
                            screen.leave();
                            #[cfg(unix)]
                            unsafe {
                                libc::raise(libc::SIGTSTP); // stopped here until `fg`
                            }
                            screen.resume()?;
                        }
                        Action::None => {}
                    }
                }
                Event::Resize(..) => forced = true,
                _ => {}
            }
        }
        let paused = pollers.paused.load(Ordering::Relaxed);
        let state: u64 = pollers
            .views
            .iter()
            .map(|v| v.lock().unwrap().version)
            .sum();
        let now_size = terminal::size().unwrap_or((80, 24));
        let since = drawn_at.elapsed();
        if forced
            || now_size != size
            || (!paused
                && ((state != drawn && since >= Duration::from_secs(1))
                    || since >= Duration::from_secs(10)))
        {
            let views: Vec<View> = pollers
                .views
                .iter()
                .map(|v| v.lock().unwrap().clone())
                .collect();
            screen.term.draw(|f| {
                let area = f.area();
                app.draw(&views, paused, &me, area, f.buffer_mut());
            })?;
            drawn = state;
            drawn_at = Instant::now();
            size = now_size;
        }
    }
    drop(screen);
    rt.shutdown_timeout(Duration::from_millis(500)); // dropping the connections ends the agents
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;

    fn views() -> Vec<View> {
        let mut out = Vec::new();
        for (i, e) in crate::demo::entries().iter().enumerate() {
            let mut d = crate::demo::DemoServer::new(e, i as u64);
            let mut v = View::new(&d.name, &d.host);
            v.apply(d.poll(), Instant::now(), 1.0);
            out.push(v);
        }
        out
    }

    fn frame(app: &mut App, w: u16, h: u16) -> String {
        let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
        app.draw(&views(), false, "nobody", buf.area, &mut buf);
        render::to_ansi(&buf, false)
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn too_tall_switches_to_compact() {
        let mut app = App::new(3.0, false, None);
        let out = frame(&mut app, 120, 30);
        assert!(app.showing_compact && out.contains("c full") && out.contains("Who (GPUs)"));
        let mut tall = App::new(3.0, false, None);
        let out = frame(&mut tall, 240, 200);
        assert!(!tall.showing_compact && out.contains("Users (GPU mem)"));
    }

    #[test]
    fn keys_scroll_toggle_and_help() {
        let mut app = App::new(3.0, false, Some(false));
        frame(&mut app, 120, 20);
        app.key(key('G'));
        frame(&mut app, 120, 20);
        assert!(app.offset > 0);
        app.key(key('g'));
        assert_eq!(app.offset, 0);
        app.key(key('c'));
        assert_eq!(app.compact, Some(true));
        assert!(matches!(app.key(key(' ')), Action::TogglePause));
        app.key(key('?'));
        assert!(frame(&mut app, 120, 20).contains("this help"));
        assert!(matches!(app.key(key('x')), Action::None) && !app.help);
        assert!(matches!(app.key(key('q')), Action::Quit));
    }

    #[test]
    fn narrow_terminal_header_keeps_hints() {
        let mut app = App::new(3.0, false, None);
        let out = frame(&mut app, 80, 24);
        let head = out.lines().next().unwrap();
        assert!(
            head.contains("4/4 up") && head.contains("? help"),
            "{}",
            head
        );
    }
}
