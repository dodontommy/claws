use crate::client;
use crate::protocol::SessionInfo;
use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use std::io::Stdout;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const PTY_ROWS: u16 = 24;
const PTY_COLS: u16 = 80;

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

enum View {
    Dashboard,
    Attached {
        session_id: Uuid,
        parser: vt100::Parser,
        read_seq: u64,
        prefix_active: bool,
    },
}

struct App {
    sessions: Vec<SessionInfo>,
    selected: usize,
    status_message: Option<(String, SystemTime)>,
    quit: bool,
    view: View,
}

impl App {
    fn new() -> Self {
        Self {
            sessions: vec![],
            selected: 0,
            status_message: None,
            quit: false,
            view: View::Dashboard,
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

    fn attached_session_id(&self) -> Option<Uuid> {
        match &self.view {
            View::Attached { session_id, .. } => Some(*session_id),
            View::Dashboard => None,
        }
    }
}

async fn run_inner(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let mut app = App::new();
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));

    refresh_sessions(&mut app).await;
    terminal.draw(|f| draw(f, &app))?;

    while !app.quit {
        tokio::select! {
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(key, &mut app).await
                    }
                    Some(Ok(Event::Resize(_, _))) => {}
                    _ => {}
                }
            }
            _ = tick.tick() => {
                tick_work(&mut app).await;
            }
        }
        app.clear_old_status();
        terminal.draw(|f| draw(f, &app))?;
    }

    Ok(())
}

async fn tick_work(app: &mut App) {
    // Attached view: high-frequency PTY-bytes poll
    if let View::Attached {
        session_id,
        parser,
        read_seq,
        ..
    } = &mut app.view
    {
        if let Ok((bytes, next, status)) = client::read_output_raw(*session_id, *read_seq).await {
            if !bytes.is_empty() {
                parser.process(&bytes);
            }
            *read_seq = next;
            // If session exited, just stay attached and let the user detach.
            let _ = status;
        }
    }

    // Lower-frequency dashboard refresh — do it every ~10 ticks (500ms).
    static mut TICK_COUNT: u32 = 0;
    let do_list = unsafe {
        TICK_COUNT = TICK_COUNT.wrapping_add(1);
        TICK_COUNT % 10 == 0
    };
    if do_list {
        refresh_sessions(app).await;
    }
}

async fn refresh_sessions(app: &mut App) {
    match client::list_sessions_raw().await {
        Ok(list) => {
            app.sessions = list;
            if !app.sessions.is_empty() && app.selected >= app.sessions.len() {
                app.selected = app.sessions.len() - 1;
            }
        }
        Err(e) => app.set_status(format!("daemon error: {e}")),
    }
}

async fn handle_key(key: KeyEvent, app: &mut App) {
    match &mut app.view {
        View::Dashboard => handle_dashboard_key(key, app).await,
        View::Attached { .. } => handle_attached_key(key, app).await,
    }
}

async fn handle_dashboard_key(key: KeyEvent, app: &mut App) {
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
        (KeyCode::Enter, KeyModifiers::NONE) => {
            if let Some(s) = app.sessions.get(app.selected) {
                if s.status != "exited" {
                    app.view = View::Attached {
                        session_id: s.id,
                        parser: vt100::Parser::new(PTY_ROWS, PTY_COLS, 0),
                        read_seq: 0,
                        prefix_active: false,
                    };
                } else {
                    app.set_status("session exited; can't attach".into());
                }
            }
        }
        _ => {}
    }
}

async fn handle_attached_key(key: KeyEvent, app: &mut App) {
    let session_id = match app.attached_session_id() {
        Some(id) => id,
        None => return,
    };

    let (prefix_active, take_prefix) = match &mut app.view {
        View::Attached { prefix_active, .. } => (*prefix_active, prefix_active),
        _ => return,
    };

    // Ctrl-Space toggles prefix mode, unless we're already in prefix mode.
    let is_prefix_keystroke =
        key.code == KeyCode::Char(' ') && key.modifiers.contains(KeyModifiers::CONTROL);

    if !prefix_active && is_prefix_keystroke {
        *take_prefix = true;
        return;
    }

    if prefix_active {
        // Consume one prefix command, then reset.
        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), _) => {
                app.view = View::Dashboard;
                app.set_status("detached".into());
                return;
            }
            (KeyCode::Char('q'), _) => {
                app.quit = true;
                return;
            }
            (KeyCode::Char('?'), _) => {
                app.set_status("prefix: d=detach q=quit (more in v0.7)".into());
            }
            (KeyCode::Char(' '), m) if m.contains(KeyModifiers::CONTROL) => {
                // Literal Ctrl-Space passthrough
                let _ = client::send_input_raw(session_id, b"\x00".to_vec()).await;
            }
            _ => {}
        }
        if let View::Attached { prefix_active, .. } = &mut app.view {
            *prefix_active = false;
        }
        return;
    }

    // Not a prefix interaction — encode the key and forward to PTY.
    let bytes = encode_key(&key);
    if !bytes.is_empty() {
        let _ = client::send_input_raw(session_id, bytes).await;
    }
}

fn encode_key(key: &KeyEvent) -> Vec<u8> {
    use KeyCode::*;
    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);

    let mut out: Vec<u8> = Vec::new();
    if alt {
        out.push(0x1b); // ESC prefix for Meta/Alt
    }
    match key.code {
        Char(c) => {
            if ctrl {
                let upper = c.to_ascii_uppercase();
                if ('@'..='_').contains(&upper) {
                    out.push((upper as u8) & 0x1f);
                } else if c == ' ' {
                    out.push(0x00); // Ctrl-Space → NUL
                } else {
                    // Unmapped Ctrl combo — pass literal char.
                    out.extend(c.to_string().as_bytes());
                }
            } else {
                out.extend(c.to_string().as_bytes());
            }
        }
        Enter => out.push(b'\r'),
        Tab => out.push(b'\t'),
        BackTab => out.extend_from_slice(b"\x1b[Z"),
        Backspace => out.push(0x7f),
        Esc => out.push(0x1b),
        Up => out.extend_from_slice(b"\x1b[A"),
        Down => out.extend_from_slice(b"\x1b[B"),
        Right => out.extend_from_slice(b"\x1b[C"),
        Left => out.extend_from_slice(b"\x1b[D"),
        Home => out.extend_from_slice(b"\x1b[H"),
        End => out.extend_from_slice(b"\x1b[F"),
        PageUp => out.extend_from_slice(b"\x1b[5~"),
        PageDown => out.extend_from_slice(b"\x1b[6~"),
        Insert => out.extend_from_slice(b"\x1b[2~"),
        Delete => out.extend_from_slice(b"\x1b[3~"),
        F(n) => {
            let seq: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => b"",
            };
            out.extend_from_slice(seq);
        }
        _ => {}
    }
    out
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    match &app.view {
        View::Dashboard => draw_dashboard(f, app),
        View::Attached { .. } => draw_attached(f, app),
    }
}

fn draw_dashboard(f: &mut ratatui::Frame, app: &App) {
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
    f.render_widget(
        Paragraph::new(title)
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    if app.sessions.is_empty() {
        f.render_widget(
            Paragraph::new("\n\n  No sessions. Press 'c' to create one in the current directory.\n")
                .style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );
    } else {
        draw_cards(f, &app.sessions, app.selected, chunks[1]);
    }

    let status_text = if let Some((msg, _)) = &app.status_message {
        format!(" {msg}")
    } else {
        " j/k move · Enter attach · c new · x close · r refresh · q quit".to_string()
    };
    f.render_widget(
        Paragraph::new(status_text).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
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
        Style::default()
            .fg(border_color)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
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
    let line2 = s
        .last_message
        .as_deref()
        .unwrap_or("(no messages yet)")
        .to_string();
    let line3 = format!(
        "turns {} · in {} out {} cache {}",
        s.turn_count, s.tokens_input, s.tokens_output, s.tokens_cache_read
    );
    f.render_widget(
        Paragraph::new(format!("{line1}\n{line2}\n{line3}"))
            .style(Style::default().fg(Color::Gray)),
        inner,
    );
}

fn draw_attached(f: &mut ratatui::Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let (session_id, parser, prefix_active) = match &app.view {
        View::Attached {
            session_id,
            parser,
            prefix_active,
            ..
        } => (*session_id, parser, *prefix_active),
        _ => return,
    };

    // Header — find the session info for richer display
    let info = app.sessions.iter().find(|s| s.id == session_id);
    let header = if let Some(s) = info {
        let display_name = s.ai_title.as_deref().unwrap_or(&s.name);
        format!(
            " ⤓ attached — {} · {} ",
            display_name,
            s.status
        )
    } else {
        format!(" ⤓ attached — {session_id} ")
    };
    f.render_widget(
        Paragraph::new(header)
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    // PTY pane — centered if user terminal is bigger than 80×24
    render_pty(f, parser, chunks[1]);

    // Footer
    let footer = if prefix_active {
        " prefix: d=detach q=quit ?=help — waiting for next key…".to_string()
    } else {
        " Ctrl-Space then d=detach · q=quit · keys → claude".to_string()
    };
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(if prefix_active {
            Color::Yellow
        } else {
            Color::DarkGray
        })),
        chunks[2],
    );
}

fn render_pty(f: &mut ratatui::Frame, parser: &vt100::Parser, area: Rect) {
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let pane_w = cols;
    let pane_h = rows;
    let x_off = area.x + area.width.saturating_sub(pane_w) / 2;
    let y_off = area.y + area.height.saturating_sub(pane_h) / 2;

    let buf = f.buffer_mut();
    for r in 0..pane_h {
        if y_off + r >= area.y + area.height {
            break;
        }
        for c in 0..pane_w {
            if x_off + c >= area.x + area.width {
                break;
            }
            let cell = match screen.cell(r, c) {
                Some(cell) => cell,
                None => continue,
            };
            let target = match buf.cell_mut(Position::new(x_off + c, y_off + r)) {
                Some(t) => t,
                None => continue,
            };
            let contents = cell.contents();
            let symbol = if contents.is_empty() { " " } else { &contents };
            target.set_symbol(symbol);
            let mut style = Style::default();
            if let Some(fg) = vt100_to_ratatui_color(cell.fgcolor()) {
                style = style.fg(fg);
            }
            if let Some(bg) = vt100_to_ratatui_color(cell.bgcolor()) {
                style = style.bg(bg);
            }
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                style = style.add_modifier(Modifier::REVERSED);
            }
            target.set_style(style);
        }
    }

    if !screen.hide_cursor() {
        let (cur_row, cur_col) = screen.cursor_position();
        if cur_row < pane_h && cur_col < pane_w {
            f.set_cursor_position(Position::new(x_off + cur_col, y_off + cur_row));
        }
    }
}

fn vt100_to_ratatui_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(idx_color(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

fn idx_color(i: u8) -> Color {
    match i {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        15 => Color::White,
        n => Color::Indexed(n),
    }
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
