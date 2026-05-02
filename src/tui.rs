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
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use std::io::Stdout;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

// Reserve 1 row each for header and footer in attached view.
const ATTACHED_CHROME_ROWS: u16 = 2;

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
    tick_phase: u32,
}

impl App {
    fn new() -> Self {
        Self {
            sessions: vec![],
            selected: 0,
            status_message: None,
            quit: false,
            view: View::Dashboard,
            tick_phase: 0,
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
                    Some(Ok(Event::Resize(cols, rows))) => {
                    if let View::Attached { session_id, parser, .. } = &mut app.view {
                        let pane_rows = rows.saturating_sub(ATTACHED_CHROME_ROWS).max(1);
                        let pane_cols = cols.max(1);
                        let _ = client::resize_session_raw(*session_id, pane_rows, pane_cols).await;
                        parser.set_size(pane_rows, pane_cols);
                    }
                }
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
    app.tick_phase = app.tick_phase.wrapping_add(1);

    // Attached view: high-frequency PTY-bytes poll.
    if let View::Attached {
        session_id,
        parser,
        read_seq,
        ..
    } = &mut app.view
    {
        if let Ok((bytes, next, _status)) = client::read_output_raw(*session_id, *read_seq).await {
            if !bytes.is_empty() {
                parser.process(&bytes);
            }
            *read_seq = next;
        }
    }

    // Lower-frequency dashboard refresh: every 10 ticks (500ms).
    if app.tick_phase % 10 == 0 {
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
                    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                    let pane_rows = rows.saturating_sub(ATTACHED_CHROME_ROWS).max(1);
                    let pane_cols = cols.max(1);
                    let _ = client::resize_session_raw(s.id, pane_rows, pane_cols).await;
                    app.view = View::Attached {
                        session_id: s.id,
                        parser: vt100::Parser::new(pane_rows, pane_cols, 0),
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
            Constraint::Length(2), // title + separator
            Constraint::Min(0),    // body
            Constraint::Length(2), // separator + footer
        ])
        .split(f.area());

    // Title row: bold "claws" + dim count + right-aligned cwd hint of selected card
    let count_part = format!(
        "{} session{}",
        app.sessions.len(),
        if app.sessions.len() == 1 { "" } else { "s" }
    );
    let cwd_hint = app
        .sessions
        .get(app.selected)
        .map(|s| s.cwd.as_str())
        .unwrap_or("");
    let title_w = chunks[0].width as usize;
    let left = format!(" claws  ╲  {count_part}");
    let title_line = if cwd_hint.is_empty() {
        left
    } else {
        let avail = title_w.saturating_sub(left.chars().count() + 2);
        let cwd_short = truncate_ellipsis(cwd_hint, avail);
        let pad = title_w
            .saturating_sub(left.chars().count() + cwd_short.chars().count())
            .saturating_sub(1);
        format!("{left}{}{cwd_short} ", " ".repeat(pad))
    };
    f.render_widget(
        Paragraph::new(title_line)
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Rect::new(chunks[0].x, chunks[0].y, chunks[0].width, 1),
    );
    f.render_widget(
        Paragraph::new("─".repeat(title_w)).style(Style::default().fg(Color::DarkGray)),
        Rect::new(chunks[0].x, chunks[0].y + 1, chunks[0].width, 1),
    );

    if app.sessions.is_empty() {
        draw_empty_state(f, chunks[1]);
    } else {
        draw_cards(f, &app.sessions, app.selected, app.tick_phase, chunks[1]);
    }

    // Footer separator + help/status line
    let footer_w = chunks[2].width as usize;
    f.render_widget(
        Paragraph::new("─".repeat(footer_w)).style(Style::default().fg(Color::DarkGray)),
        Rect::new(chunks[2].x, chunks[2].y, chunks[2].width, 1),
    );
    let status_text = if let Some((msg, _)) = &app.status_message {
        format!(" {msg}")
    } else {
        " j/k  move    enter  attach    c  new    x  close    r  refresh    q  quit".to_string()
    };
    f.render_widget(
        Paragraph::new(status_text).style(Style::default().fg(Color::DarkGray)),
        Rect::new(chunks[2].x, chunks[2].y + 1, chunks[2].width, 1),
    );
}

fn draw_empty_state(f: &mut ratatui::Frame, area: Rect) {
    let lines = vec![
        "".to_string(),
        "".to_string(),
        "◌".to_string(),
        "".to_string(),
        "no sessions yet".to_string(),
        "".to_string(),
        "press  c  to create one here".to_string(),
    ];
    let h = lines.len() as u16;
    let y = area.y + area.height.saturating_sub(h) / 2;
    for (i, line) in lines.iter().enumerate() {
        let w = line.chars().count() as u16;
        let x = area.x + area.width.saturating_sub(w) / 2;
        let style = match i {
            2 => Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
            4 => Style::default().fg(Color::Gray),
            _ => Style::default().fg(Color::DarkGray),
        };
        f.render_widget(
            Paragraph::new(line.clone()).style(style),
            Rect::new(x, y + i as u16, w, 1),
        );
    }
}

const CARD_W: u16 = 40;
const CARD_H: u16 = 6;

fn draw_cards(
    f: &mut ratatui::Frame,
    sessions: &[SessionInfo],
    selected: usize,
    tick_phase: u32,
    area: Rect,
) {
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
        draw_card(f, s, idx == selected, tick_phase, card_area);
    }
}

fn status_color(status: &str) -> Color {
    match status {
        "spawning" => Color::DarkGray,
        "idle" => Color::Green,
        "streaming" => Color::Yellow,
        "awaiting_permission" => Color::LightMagenta,
        "exited" => Color::DarkGray,
        _ => Color::White,
    }
}

fn status_glyph(status: &str, tick_phase: u32) -> &'static str {
    match status {
        "spawning" => "◌",
        "idle" => "●",
        "streaming" => {
            const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
            FRAMES[((tick_phase / 3) % 4) as usize]
        }
        "awaiting_permission" => "★",
        "exited" => "○",
        _ => "?",
    }
}

fn draw_card(
    f: &mut ratatui::Frame,
    s: &SessionInfo,
    selected: bool,
    tick_phase: u32,
    area: Rect,
) {
    let color = status_color(&s.status);
    let border_type = if selected { BorderType::Double } else { BorderType::Plain };
    let border_style = if selected {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color)
    };

    let glyph = status_glyph(&s.status, tick_phase);
    let display_name = s.ai_title.as_deref().unwrap_or(&s.name);
    let title_text = format!(" {glyph}  {display_name} ");

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(border_type)
        .border_style(border_style)
        .title(ratatui::text::Span::styled(
            title_text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    use ratatui::text::{Line, Span};

    // Inner padding: 1 char left margin
    let pad = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );

    let time_ago = format_time_ago(s.last_activity_ms);
    let status_label = display_status(&s.status);
    let tool_part = s.current_tool.as_deref().map(|t| format!("  ·  {t}")).unwrap_or_default();
    let exit_part = s.exit_code.map(|c| format!("  ·  exit {c}")).unwrap_or_default();

    let line1 = Line::from(vec![
        Span::styled(status_label, Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  ·  {time_ago}"), Style::default().fg(Color::Gray)),
        Span::styled(tool_part, Style::default().fg(Color::Cyan)),
        Span::styled(exit_part, Style::default().fg(Color::DarkGray)),
    ]);

    let preview_avail = pad.width.saturating_sub(0) as usize;
    let preview = match s.last_message.as_deref() {
        Some(m) if !m.is_empty() => truncate_ellipsis(m, preview_avail),
        _ => "(no messages yet)".to_string(),
    };
    let preview_style = if s.last_message.is_some() {
        Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)
    };
    let line2 = Line::from(Span::styled(preview, preview_style));

    let model = s.model.as_deref().map(short_model).unwrap_or("—");
    let toks = format!(
        "{}t  ·  {}→{}  ·  ◷ {}",
        s.turn_count,
        compact_num(s.tokens_input),
        compact_num(s.tokens_output),
        compact_num(s.tokens_cache_read),
    );
    let line3 = Line::from(vec![
        Span::styled(model.to_string(), Style::default().fg(Color::Magenta)),
        Span::styled(format!("  ·  {toks}"), Style::default().fg(Color::DarkGray)),
    ]);

    let p = Paragraph::new(vec![line1, line2, line3]).wrap(Wrap { trim: false });
    f.render_widget(p, pad);
}

fn display_status(s: &str) -> &'static str {
    match s {
        "spawning" => "spawning",
        "idle" => "idle",
        "streaming" => "streaming",
        "awaiting_permission" => "needs you",
        "exited" => "exited",
        _ => "?",
    }
}

fn truncate_ellipsis(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let take = max.saturating_sub(1);
    let mut out: String = chars.into_iter().take(take).collect();
    out.push('…');
    out
}

fn compact_num(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{}k", n / 1000)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
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

    // Header — name, status with color, time-ago, model
    let info = app.sessions.iter().find(|s| s.id == session_id);
    use ratatui::text::{Line, Span};
    let header_line = if let Some(s) = info {
        let color = status_color(&s.status);
        let glyph = status_glyph(&s.status, app.tick_phase);
        let name = s.ai_title.as_deref().unwrap_or(&s.name);
        let model = s.model.as_deref().map(short_model).unwrap_or("");
        let time_ago = format_time_ago(s.last_activity_ms);
        Line::from(vec![
            Span::styled(format!(" {glyph}  "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(name.to_string(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  ·  {}", display_status(&s.status)), Style::default().fg(color)),
            Span::styled(format!("  ·  {time_ago}"), Style::default().fg(Color::Gray)),
            Span::styled(format!("  ·  {model}"), Style::default().fg(Color::Magenta)),
        ])
    } else {
        Line::from(Span::styled(format!(" attached — {session_id} "), Style::default().fg(Color::Cyan)))
    };
    f.render_widget(Paragraph::new(header_line), chunks[0]);

    render_pty(f, parser, chunks[1]);

    let footer = if prefix_active {
        " ◆ prefix:  d  detach    q  quit    ?  help    Ctrl-Space  literal".to_string()
    } else {
        " Ctrl-Space  prefix    keys → claude".to_string()
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
    let buf = f.buffer_mut();
    for r in 0..rows {
        if r >= area.height {
            break;
        }
        for c in 0..cols {
            if c >= area.width {
                break;
            }
            let cell = match screen.cell(r, c) {
                Some(cell) => cell,
                None => continue,
            };
            let target = match buf.cell_mut(Position::new(area.x + c, area.y + r)) {
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
        if cur_row < rows.min(area.height) && cur_col < cols.min(area.width) {
            f.set_cursor_position(Position::new(area.x + cur_col, area.y + cur_row));
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
    if seconds < 2 {
        "now".to_string()
    } else if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86400)
    }
}
