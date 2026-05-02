use crate::client;
use crate::protocol::SessionInfo;
use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use std::io::Stdout;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub async fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_inner(&mut terminal).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

struct App {
    sessions: Vec<SessionInfo>,
    selected: usize,
    status_message: Option<(String, SystemTime)>,
    quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            sessions: vec![],
            selected: 0,
            status_message: None,
            quit: false,
        }
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.sessions.len() {
            self.selected += 1;
        }
    }

    fn set_status(&mut self, msg: String) {
        self.status_message = Some((msg, SystemTime::now()));
    }

    fn clear_old_status(&mut self) {
        if let Some((_, t)) = self.status_message.as_ref() {
            if t.elapsed().unwrap_or_default() > Duration::from_secs(3) {
                self.status_message = None;
            }
        }
    }
}

async fn run_inner(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let mut app = App::new();
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));

    refresh_sessions(&mut app).await;
    terminal.draw(|f| draw(f, &app))?;

    while !app.quit {
        tokio::select! {
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(key))) => handle_key(key, &mut app).await,
                    Some(Ok(Event::Resize(_, _))) => {}
                    _ => {}
                }
            }
            _ = tick.tick() => {
                refresh_sessions(&mut app).await;
            }
        }
        app.clear_old_status();
        terminal.draw(|f| draw(f, &app))?;
    }

    Ok(())
}

async fn refresh_sessions(app: &mut App) {
    match client::list_sessions_raw().await {
        Ok(list) => {
            app.sessions = list;
            if !app.sessions.is_empty() && app.selected >= app.sessions.len() {
                app.selected = app.sessions.len() - 1;
            }
        }
        Err(e) => {
            app.set_status(format!("daemon error: {e}"));
        }
    }
}

async fn handle_key(key: KeyEvent, app: &mut App) {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), KeyModifiers::NONE) => app.quit = true,
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => app.quit = true,
        (KeyCode::Char('j'), _) | (KeyCode::Down, _) => app.move_down(),
        (KeyCode::Char('k'), _) | (KeyCode::Up, _) => app.move_up(),
        (KeyCode::Char('r'), KeyModifiers::NONE) => {
            refresh_sessions(app).await;
            app.set_status("refreshed".into());
        }
        (KeyCode::Char('c'), KeyModifiers::NONE) => {
            let cwd = match std::env::current_dir() {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(e) => {
                    app.set_status(format!("cwd error: {e}"));
                    return;
                }
            };
            match client::create_session_raw(cwd, None, None).await {
                Ok(_) => {
                    app.set_status("session created".into());
                    refresh_sessions(app).await;
                }
                Err(e) => app.set_status(format!("create failed: {e}")),
            }
        }
        (KeyCode::Char('x'), KeyModifiers::NONE) => {
            if let Some(s) = app.sessions.get(app.selected) {
                let id = s.id.to_string();
                match client::close_session_raw(id).await {
                    Ok(_) => {
                        app.set_status("closed".into());
                        refresh_sessions(app).await;
                    }
                    Err(e) => app.set_status(format!("close failed: {e}")),
                }
            }
        }
        _ => {}
    }
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    let title = format!(
        " claws — {} session{}",
        app.sessions.len(),
        if app.sessions.len() == 1 { "" } else { "s" }
    );
    let title_p = Paragraph::new(title)
        .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD));
    f.render_widget(title_p, chunks[0]);

    if app.sessions.is_empty() {
        let empty = Paragraph::new("\n\n  No sessions. Press 'c' to create one in the current directory.\n")
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(empty, chunks[1]);
    } else {
        draw_cards(f, &app.sessions, app.selected, chunks[1]);
    }

    let status_text = if let Some((msg, _)) = &app.status_message {
        format!(" {msg}")
    } else {
        " j/k move · c new · x close · r refresh · q quit".to_string()
    };
    let help_p = Paragraph::new(status_text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(help_p, chunks[2]);
}

const CARD_W: u16 = 38;
const CARD_H: u16 = 5;

fn draw_cards(f: &mut ratatui::Frame, sessions: &[SessionInfo], selected: usize, area: Rect) {
    let cols = ((area.width / CARD_W).max(1)) as usize;
    for (idx, s) in sessions.iter().enumerate() {
        let row = (idx / cols) as u16;
        let col = (idx % cols) as u16;
        let x = area.x + col * CARD_W;
        let y = area.y + row * CARD_H;
        if y + CARD_H > area.y + area.height {
            break;
        }
        let w = CARD_W.min(area.width.saturating_sub(col * CARD_W));
        let card_area = Rect::new(x, y, w, CARD_H);
        draw_card(f, s, idx == selected, card_area);
    }
}

fn draw_card(f: &mut ratatui::Frame, s: &SessionInfo, selected: bool, area: Rect) {
    let border_color = match s.status.as_str() {
        "spawning" => Color::DarkGray,
        "idle" => Color::Green,
        "streaming" => Color::Yellow,
        "awaiting_permission" => Color::Red,
        "exited" => Color::DarkGray,
        _ => Color::White,
    };
    let border_style = if selected {
        Style::default().fg(border_color).add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(border_color)
    };

    let glyph = match s.status.as_str() {
        "spawning" => "◌",
        "idle" => "●",
        "streaming" => "◐",
        "awaiting_permission" => "★",
        "exited" => "○",
        _ => "?",
    };
    let display_name = s.ai_title.as_deref().unwrap_or(&s.name);
    let model_short = s.model.as_deref().map(short_model).unwrap_or("");
    let title = format!(" {glyph} {display_name} — {model_short} ");

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let time_ago = format_time_ago(s.last_activity_ms);
    let tool_part = s
        .current_tool
        .as_deref()
        .map(|t| format!(" · {t}"))
        .unwrap_or_default();
    let exit_part = s.exit_code.map(|c| format!(" · exit {c}")).unwrap_or_default();
    let line1 = format!("{} · {}{}{}", s.status, time_ago, tool_part, exit_part);
    let line2 = s.last_message.as_deref().unwrap_or("(no messages yet)").to_string();
    let line3 = format!(
        "turns {} · in {} out {} cache {}",
        s.turn_count, s.tokens_input, s.tokens_output, s.tokens_cache_read
    );

    let p = Paragraph::new(format!("{line1}\n{line2}\n{line3}"))
        .style(Style::default().fg(Color::Gray));
    f.render_widget(p, inner);
}

fn short_model(m: &str) -> &str {
    if m.contains("opus") {
        "opus"
    } else if m.contains("sonnet") {
        "sonnet"
    } else if m.contains("haiku") {
        "haiku"
    } else {
        m
    }
}

fn format_time_ago(ms_since_epoch: u128) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let elapsed_ms = now.saturating_sub(ms_since_epoch);
    let seconds = elapsed_ms / 1000;
    if seconds < 60 {
        format!("{}s ago", seconds)
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}
