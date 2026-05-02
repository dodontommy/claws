use crate::client;
use crate::protocol::SessionInfo;
use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;
use std::io::Stdout;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

// Reserve 1 row each for header and footer in attached view.
const ATTACHED_CHROME_ROWS: u16 = 2;

pub async fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_inner(&mut terminal).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

struct ScrollState {
    bytes: Vec<u8>,
    /// Number of bytes from the start of `bytes` to feed the ephemeral
    /// parser. `offset == bytes.len()` means "current state" (latest).
    offset: usize,
}

enum View {
    Dashboard,
    Attached {
        session_id: Uuid,
        parser: vt100::Parser,
        read_seq: u64,
        prefix_active: bool,
        scroll: Option<ScrollState>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormField {
    Cwd,
    Args,
}

enum Modal {
    SpawnForm {
        cwd: String,
        cwd_cursor: usize,
        args: String,
        args_cursor: usize,
        focus: FormField,
        recent_selected: usize,
    },
    Rename {
        session_id: Uuid,
        input: String,
        cursor: usize,
    },
    Details {
        session_id: Uuid,
    },
    Help,
}

struct App {
    sessions: Vec<SessionInfo>,
    selected: usize,
    status_message: Option<(String, SystemTime)>,
    quit: bool,
    view: View,
    tick_phase: u32,
    modal: Option<Modal>,
    recent_cwds: Vec<String>,
    grid_cols: u16,
    filter: Option<String>,
    filter_cursor: usize,
    last_click: Option<(usize, SystemTime)>,
    /// Vertical scroll offset for the detail pane's last-message paragraph.
    /// Reset to 0 whenever the selected session changes.
    detail_scroll: u16,
}

impl App {
    fn new() -> Self {
        let pwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut recent = Vec::new();
        if !pwd.is_empty() {
            recent.push(pwd);
        }
        Self {
            sessions: vec![],
            selected: 0,
            status_message: None,
            quit: false,
            view: View::Dashboard,
            tick_phase: 0,
            modal: None,
            recent_cwds: recent,
            grid_cols: 1,
            filter: None,
            filter_cursor: 0,
            last_click: None,
            detail_scroll: 0,
        }
    }

    fn select(&mut self, idx: usize) {
        if idx != self.selected {
            self.selected = idx;
            self.detail_scroll = 0;
        }
    }

    /// Sessions filtered by current filter string. Returned as references.
    fn visible_sessions(&self) -> Vec<&SessionInfo> {
        match &self.filter {
            None => self.sessions.iter().collect(),
            Some(f) if f.is_empty() => self.sessions.iter().collect(),
            Some(f) => {
                let needle = f.to_lowercase();
                self.sessions
                    .iter()
                    .filter(|s| {
                        s.name.to_lowercase().contains(&needle)
                            || s.cwd.to_lowercase().contains(&needle)
                            || s.ai_title.as_deref().map(|t| t.to_lowercase().contains(&needle)).unwrap_or(false)
                            || s.display_override.as_deref().map(|d| d.to_lowercase().contains(&needle)).unwrap_or(false)
                    })
                    .collect()
            }
        }
    }

    /// Map dashboard `selected` index to the actual SessionInfo via the visible list.
    fn selected_session(&self) -> Option<SessionInfo> {
        self.visible_sessions().get(self.selected).map(|s| (*s).clone())
    }

    fn push_recent_cwd(&mut self, cwd: String) {
        self.recent_cwds.retain(|c| c != &cwd);
        self.recent_cwds.insert(0, cwd);
        if self.recent_cwds.len() > 20 {
            self.recent_cwds.truncate(20);
        }
    }

    fn visible_count(&self) -> usize {
        self.visible_sessions().len()
    }

    fn move_up(&mut self) {
        let cols = self.grid_cols.max(1) as usize;
        if self.selected >= cols {
            self.select(self.selected - cols);
        }
    }
    fn move_down(&mut self) {
        let cols = self.grid_cols.max(1) as usize;
        let target = self.selected + cols;
        if target < self.visible_count() {
            self.select(target);
        }
    }
    fn move_left(&mut self) {
        if self.selected > 0 {
            self.select(self.selected - 1);
        }
    }
    fn move_right(&mut self) {
        if self.selected + 1 < self.visible_count() {
            self.select(self.selected + 1);
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
        Ok(mut list) => {
            // Sort: needs-you first, then streaming, then idle by recent activity,
            // spawning, exited last. Within a status bucket, most-recent first.
            list.sort_by(|a, b| {
                sort_priority(&a.status)
                    .cmp(&sort_priority(&b.status))
                    .then_with(|| b.last_activity_ms.cmp(&a.last_activity_ms))
            });
            app.sessions = list;
            if !app.sessions.is_empty() && app.selected >= app.sessions.len() {
                app.selected = app.sessions.len() - 1;
            }
        }
        Err(e) => app.set_status(format!("daemon error: {e}")),
    }
}

fn sort_priority(status: &str) -> u8 {
    match status {
        "awaiting_permission" => 0,
        "streaming" => 1,
        "idle" => 2,
        "spawning" => 3,
        "exited" => 4,
        _ => 5,
    }
}

async fn handle_key(key: KeyEvent, app: &mut App) {
    if app.modal.is_some() {
        handle_modal_key(key, app).await;
        return;
    }
    match &mut app.view {
        View::Dashboard => handle_dashboard_key(key, app).await,
        View::Attached { .. } => handle_attached_key(key, app).await,
    }
}

async fn handle_mouse(ev: MouseEvent, app: &mut App) {
    // Only act on dashboard for now; attached view forwards mouse to PTY in v.next.
    if !matches!(app.view, View::Dashboard) || app.modal.is_some() {
        return;
    }
    match ev.kind {
        MouseEventKind::ScrollUp => app.move_up(),
        MouseEventKind::ScrollDown => app.move_down(),
        MouseEventKind::Down(MouseButton::Left) => {
            // Body area starts at row 2 (after title + separator).
            // Sidebar layout: each session entry is SIDEBAR_ROW_H rows.
            // Clicks in the sidebar select; clicks in the detail pane do nothing.
            let body_y0 = 2u16;
            if ev.row < body_y0 {
                return;
            }
            if ev.column >= SIDEBAR_W {
                return;
            }
            let row = (ev.row - body_y0) / SIDEBAR_ROW_H;
            let idx = row as usize;
            if idx >= app.visible_count() {
                return;
            }
            let now = SystemTime::now();
            let dbl = match app.last_click {
                Some((prev_idx, t))
                    if prev_idx == idx
                        && now.duration_since(t).unwrap_or_default()
                            < Duration::from_millis(500) =>
                {
                    true
                }
                _ => false,
            };
            app.select(idx);
            app.last_click = Some((idx, now));
            if dbl {
                attach_at_index(app, idx).await;
            }
        }
        _ => {}
    }
}

async fn handle_modal_key(key: KeyEvent, app: &mut App) {
    // Help modal: any key closes it.
    if matches!(app.modal, Some(Modal::Help)) {
        app.modal = None;
        return;
    }
    // Details modal: any key closes it.
    if matches!(app.modal, Some(Modal::Details { .. })) {
        app.modal = None;
        return;
    }
    // Rename modal: text input + Enter/Esc.
    if let Some(Modal::Rename { session_id, input, cursor }) = app.modal.as_mut() {
        let session_id = *session_id;
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => app.modal = None,
            (KeyCode::Enter, _) => {
                let new = input.trim().to_string();
                app.modal = None;
                match client::rename_session_raw(session_id, new).await {
                    Ok(()) => {
                        app.set_status("renamed".into());
                        refresh_sessions(app).await;
                    }
                    Err(e) => app.set_status(format!("rename failed: {e}")),
                }
            }
            _ => handle_text_input(input, cursor, &key),
        }
        return;
    }
    let modal = match app.modal.as_mut() {
        Some(m) => m,
        None => return,
    };
    match modal {
        Modal::Help | Modal::Details { .. } | Modal::Rename { .. } => return,
        Modal::SpawnForm {
            cwd,
            cwd_cursor,
            args,
            args_cursor,
            focus,
            recent_selected,
        } => match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => app.modal = None,
            (KeyCode::Enter, _) => {
                let cwd_v = cwd.trim().to_string();
                if cwd_v.is_empty() {
                    app.set_status("cwd is empty".into());
                    return;
                }
                if !PathBuf::from(&cwd_v).is_dir() {
                    app.set_status(format!("not a directory: {cwd_v}"));
                    return;
                }
                let extra: Vec<String> = match shell_words::split(args) {
                    Ok(v) => v,
                    Err(e) => {
                        app.set_status(format!("bad args: {e}"));
                        return;
                    }
                };
                app.modal = None;
                match client::create_session_raw(cwd_v.clone(), None, None, extra).await {
                    Ok(_) => {
                        app.push_recent_cwd(cwd_v);
                        app.set_status("session created".into());
                        refresh_sessions(app).await;
                    }
                    Err(e) => app.set_status(format!("create failed: {e}")),
                }
            }
            (KeyCode::Tab, _) => {
                *focus = match focus {
                    FormField::Cwd => FormField::Args,
                    FormField::Args => FormField::Cwd,
                };
            }
            (KeyCode::Char('y'), m) if m.contains(KeyModifiers::CONTROL) => {
                let flag = "--dangerously-skip-permissions";
                if args.contains(flag) {
                    let cleaned = args
                        .split_whitespace()
                        .filter(|t| *t != flag)
                        .collect::<Vec<_>>()
                        .join(" ");
                    *args = cleaned;
                } else {
                    if !args.is_empty() && !args.ends_with(' ') {
                        args.push(' ');
                    }
                    args.push_str(flag);
                }
                *args_cursor = args.len();
            }
            (KeyCode::Up, _) if *focus == FormField::Cwd => {
                if *recent_selected > 0 {
                    *recent_selected -= 1;
                    if let Some(pick) = app.recent_cwds.get(*recent_selected) {
                        *cwd = pick.clone();
                        *cwd_cursor = cwd.len();
                    }
                }
            }
            (KeyCode::Down, _) if *focus == FormField::Cwd => {
                if *recent_selected + 1 < app.recent_cwds.len() {
                    *recent_selected += 1;
                    if let Some(pick) = app.recent_cwds.get(*recent_selected) {
                        *cwd = pick.clone();
                        *cwd_cursor = cwd.len();
                    }
                }
            }
            _ => {
                let (input, cursor) = match focus {
                    FormField::Cwd => (cwd, cwd_cursor),
                    FormField::Args => (args, args_cursor),
                };
                handle_text_input(input, cursor, &key);
            }
        },
    }
}

fn handle_text_input(input: &mut String, cursor: &mut usize, key: &KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
            input.insert(*cursor, c);
            *cursor += c.len_utf8();
        }
        (KeyCode::Backspace, _) => input_backspace(input, cursor),
        (KeyCode::Delete, _) => input_delete(input, cursor),
        (KeyCode::Left, _) => input_move_left(input, cursor),
        (KeyCode::Right, _) => input_move_right(input, cursor),
        (KeyCode::Home, _) => *cursor = 0,
        (KeyCode::End, _) => *cursor = input.len(),
        _ => {}
    }
}

fn input_backspace(input: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let mut prev = 0;
    let mut i = 0;
    for ch in input.chars() {
        let next = i + ch.len_utf8();
        if next == *cursor {
            prev = i;
            break;
        }
        i = next;
    }
    input.replace_range(prev..*cursor, "");
    *cursor = prev;
}

fn input_delete(input: &mut String, cursor: &mut usize) {
    if *cursor >= input.len() {
        return;
    }
    if let Some(ch) = input[*cursor..].chars().next() {
        let end = *cursor + ch.len_utf8();
        input.replace_range(*cursor..end, "");
    }
}

fn input_move_left(input: &str, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let mut prev = 0;
    let mut i = 0;
    for ch in input.chars() {
        let next = i + ch.len_utf8();
        if next == *cursor {
            prev = i;
            break;
        }
        i = next;
    }
    *cursor = prev;
}

fn input_move_right(input: &str, cursor: &mut usize) {
    if *cursor >= input.len() {
        return;
    }
    if let Some(ch) = input[*cursor..].chars().next() {
        *cursor += ch.len_utf8();
    }
}

async fn handle_dashboard_key(key: KeyEvent, app: &mut App) {
    // Filter mode: capture all printable input until Esc/Enter.
    if app.filter.is_some() {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                app.filter = None;
                app.filter_cursor = 0;
                app.selected = 0;
            }
            (KeyCode::Enter, _) => {
                // Exit filter input, but keep filter active. Esc on an active
                // (non-typing) filter clears it — we route that through the
                // first match case the next time the filter is non-empty.
                if let Some(f) = &app.filter {
                    if f.is_empty() {
                        app.filter = None;
                    }
                }
            }
            (KeyCode::Backspace, _) => {
                if let Some(f) = app.filter.as_mut() {
                    input_backspace(f, &mut app.filter_cursor);
                }
                app.selected = 0;
            }
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                if let Some(f) = app.filter.as_mut() {
                    f.insert(app.filter_cursor, c);
                    app.filter_cursor += c.len_utf8();
                }
                app.selected = 0;
            }
            _ => {}
        }
        return;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), KeyModifiers::NONE) => app.quit = true,
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => app.quit = true,
        (KeyCode::Char('j'), _) | (KeyCode::Down, _) => app.move_down(),
        (KeyCode::Char('k'), _) | (KeyCode::Up, _) => app.move_up(),
        (KeyCode::Char('h'), _) | (KeyCode::Left, _) => app.move_left(),
        (KeyCode::Char('l'), _) | (KeyCode::Right, _) => app.move_right(),
        (KeyCode::PageDown, _) => {
            app.detail_scroll = app.detail_scroll.saturating_add(8);
        }
        (KeyCode::PageUp, _) => {
            app.detail_scroll = app.detail_scroll.saturating_sub(8);
        }
        (KeyCode::Char('J'), KeyModifiers::SHIFT) => {
            app.detail_scroll = app.detail_scroll.saturating_add(1);
        }
        (KeyCode::Char('K'), KeyModifiers::SHIFT) => {
            app.detail_scroll = app.detail_scroll.saturating_sub(1);
        }
        (KeyCode::Char('?'), _) => app.modal = Some(Modal::Help),
        (KeyCode::Char('t'), KeyModifiers::NONE) => {
            let t = crate::theme::cycle();
            app.set_status(format!("theme: {}", t.label));
        }
        (KeyCode::Char('/'), _) => {
            app.filter = Some(String::new());
            app.filter_cursor = 0;
            app.selected = 0;
        }
        (KeyCode::Char('i'), _) => {
            if let Some(s) = app.selected_session() {
                app.modal = Some(Modal::Details { session_id: s.id });
            }
        }
        (KeyCode::Char('r'), KeyModifiers::SHIFT) | (KeyCode::Char('R'), _) => {
            if let Some(s) = app.selected_session() {
                match client::restart_session_raw(s.id).await {
                    Ok(()) => {
                        app.set_status("restarted".into());
                        refresh_sessions(app).await;
                    }
                    Err(e) => app.set_status(format!("restart failed: {e}")),
                }
            }
        }
        (KeyCode::Char('r'), KeyModifiers::NONE) => {
            // Lowercase r → rename. Shift+R or capital R is restart, above.
            if let Some(s) = app.selected_session() {
                let initial = s
                    .display_override
                    .clone()
                    .or_else(|| s.ai_title.clone())
                    .unwrap_or_else(|| s.name.clone());
                let cursor = initial.len();
                app.modal = Some(Modal::Rename {
                    session_id: s.id,
                    input: initial,
                    cursor,
                });
            }
        }
        (KeyCode::F(5), _) => {
            refresh_sessions(app).await;
            app.set_status("refreshed".into());
        }
        (KeyCode::Char('c'), KeyModifiers::NONE) => {
            let pwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let cursor = pwd.len();
            app.modal = Some(Modal::SpawnForm {
                cwd: pwd,
                cwd_cursor: cursor,
                args: String::new(),
                args_cursor: 0,
                focus: FormField::Cwd,
                recent_selected: 0,
            });
        }
        (KeyCode::Char('x'), KeyModifiers::NONE) => {
            if let Some(s) = app.selected_session() {
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
            attach_at_index(app, app.selected).await;
        }
        _ => {}
    }
}

async fn attach_at_index(app: &mut App, idx: usize) {
    let info = match app.visible_sessions().get(idx) {
        Some(s) => (*s).clone(),
        None => return,
    };
    if info.status == "exited" {
        app.set_status("session exited; can't attach".into());
        return;
    }
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let pane_rows = rows.saturating_sub(ATTACHED_CHROME_ROWS).max(1);
    let pane_cols = cols.max(1);
    let _ = client::resize_session_raw(info.id, pane_rows, pane_cols).await;
    app.view = View::Attached {
        session_id: info.id,
        parser: vt100::Parser::new(pane_rows, pane_cols, 0),
        read_seq: 0,
        prefix_active: false,
        scroll: None,
    };
    app.select(idx);
}

async fn attach_at_offset(app: &mut App, offset: isize) {
    let visible = app.visible_sessions();
    let n = visible.len();
    if n == 0 {
        return;
    }
    let cur_id = app.attached_session_id();
    let cur_idx = cur_id
        .and_then(|id| visible.iter().position(|s| s.id == id))
        .unwrap_or(0);
    let new_idx = ((cur_idx as isize + offset).rem_euclid(n as isize)) as usize;
    drop(visible);
    attach_at_index(app, new_idx).await;
}

async fn handle_attached_key(key: KeyEvent, app: &mut App) {
    let session_id = match app.attached_session_id() {
        Some(id) => id,
        None => return,
    };

    // If we're in scroll mode, all keys go to the scroll-mode handler — no
    // forwarding to the PTY, no prefix interpretation.
    let in_scroll = matches!(
        &app.view,
        View::Attached { scroll: Some(_), .. }
    );
    if in_scroll {
        handle_scroll_key(key, app);
        return;
    }

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
            (KeyCode::Char('n'), _) => {
                attach_at_offset(app, 1).await;
                return;
            }
            (KeyCode::Char('p'), _) => {
                attach_at_offset(app, -1).await;
                return;
            }
            (KeyCode::Char('['), _) => {
                enter_scroll_mode(app, session_id).await;
                return;
            }
            (KeyCode::Char(c), _) if c.is_ascii_digit() && c != '0' => {
                let idx = (c.to_digit(10).unwrap() as usize) - 1;
                attach_at_index(app, idx).await;
                return;
            }
            (KeyCode::Char('?'), _) => {
                app.set_status("prefix: d=detach n/p=cycle 1-9=jump [=scroll q=quit".into());
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

/// Fetch the daemon's full ring buffer for the current session and enter
/// scroll mode positioned at "latest". User can then back up through
/// history with arrows / pageup / home.
async fn enter_scroll_mode(app: &mut App, session_id: Uuid) {
    let result = client::read_output_raw(session_id, 0).await;
    match result {
        Ok((bytes, _next_seq, _status)) => {
            let offset = bytes.len();
            if let View::Attached { scroll, .. } = &mut app.view {
                *scroll = Some(ScrollState { bytes, offset });
            }
            app.set_status("scroll mode  ·  ↑↓ jk · pgup/pgdn · g/G top/bottom · esc exit".into());
        }
        Err(e) => app.set_status(format!("scroll: fetch failed: {e}")),
    }
}

fn handle_scroll_key(key: KeyEvent, app: &mut App) {
    let scroll = match &mut app.view {
        View::Attached {
            scroll: Some(s), ..
        } => s,
        _ => return,
    };
    // Step sizes are byte-based since we replay the raw stream. ~1920 bytes
    // is roughly one 24×80 screenful of dense text — close enough for line-
    // ish scroll feel without parsing newlines out of the byte stream.
    let line = 1920usize;
    let page = line * 4;
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => {
            if let View::Attached { scroll, .. } = &mut app.view {
                *scroll = None;
            }
            app.set_status("live".into());
        }
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
            scroll.offset = scroll.offset.saturating_sub(line);
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
            scroll.offset = (scroll.offset + line).min(scroll.bytes.len());
        }
        (KeyCode::PageUp, _) => {
            scroll.offset = scroll.offset.saturating_sub(page);
        }
        (KeyCode::PageDown, _) => {
            scroll.offset = (scroll.offset + page).min(scroll.bytes.len());
        }
        (KeyCode::Home, _) | (KeyCode::Char('g'), _) => {
            scroll.offset = 0;
        }
        (KeyCode::End, _) | (KeyCode::Char('G'), _) => {
            scroll.offset = scroll.bytes.len();
        }
        _ => {}
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
    if let Some(modal) = &app.modal {
        draw_modal(f, modal, app);
    }
}

fn draw_modal(f: &mut ratatui::Frame, modal: &Modal, app: &App) {
    match modal {
        Modal::SpawnForm {
            cwd,
            cwd_cursor,
            args,
            args_cursor,
            focus,
            recent_selected,
        } => draw_spawn_form(
            f,
            cwd,
            *cwd_cursor,
            args,
            *args_cursor,
            *focus,
            *recent_selected,
            &app.recent_cwds,
        ),
        Modal::Help => draw_help(f),
        Modal::Rename { input, cursor, .. } => draw_rename(f, input, *cursor),
        Modal::Details { session_id } => {
            if let Some(s) = app.sessions.iter().find(|s| s.id == *session_id) {
                draw_details(f, s);
            } else {
                draw_help(f);
            }
        }
    }
}

fn draw_help(f: &mut ratatui::Frame) {
    let parent = f.area();
    let w = 78.min(parent.width.saturating_sub(4));
    let h = 38.min(parent.height.saturating_sub(2));
    let area = centered_rect(w, h, parent);
    f.render_widget(Clear, area);
    let title = format!(" claws {}  ·  keymap ", env!("CARGO_PKG_VERSION"));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(ratatui::text::Span::styled(
            title,
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    use ratatui::text::{Line, Span};
    let key_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::White);
    let dim_style = Style::default().fg(Color::DarkGray);

    let sec = |label: &'static str| -> Line<'static> {
        Line::from(Span::styled(
            label.to_string(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
    };
    let row = |k: &str, v: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled("  ", dim_style),
            Span::styled(format!("{k:<22}"), key_style),
            Span::styled(v.to_string(), desc_style),
        ])
    };

    let mut lines: Vec<Line> = Vec::new();

    lines.push(sec("dashboard"));
    lines.push(row("h j k l / arrows", "navigate sessions"));
    lines.push(row("enter", "attach to selected session"));
    lines.push(row("c", "new session (opens spawn form)"));
    lines.push(row("r", "rename selected"));
    lines.push(row("R", "restart selected (kill + claude --resume)"));
    lines.push(row("x", "close & forget selected"));
    lines.push(row("/", "filter sessions"));
    lines.push(row("i", "session details popup"));
    lines.push(row("F5", "force refresh"));
    lines.push(row("t", "cycle color theme"));
    lines.push(row("?", "this help"));
    lines.push(row("q", "quit TUI  (daemon and sessions stay alive)"));

    lines.push(Line::from(""));
    lines.push(sec("attached view"));
    lines.push(row("Ctrl-Space", "prefix — next key is a claws command"));
    lines.push(row("Ctrl-Space d", "detach back to dashboard"));
    lines.push(row("Ctrl-Space n / p", "next / previous session"));
    lines.push(row("Ctrl-Space 1-9", "jump to session N"));
    lines.push(row("Ctrl-Space [", "enter scroll mode  (read history)"));
    lines.push(row("Ctrl-Space q", "quit TUI"));
    lines.push(row("Ctrl-Space Ctrl-Space", "send literal Ctrl-Space to claude"));

    lines.push(Line::from(""));
    lines.push(sec("scroll mode  (Ctrl-Space [)"));
    lines.push(row("↑ / ↓  or  j / k", "scroll one screenful"));
    lines.push(row("PageUp / PageDown", "scroll several screenfuls"));
    lines.push(row("g / Home", "jump to start of history"));
    lines.push(row("G / End", "jump back to live"));
    lines.push(row("esc / q", "exit scroll mode"));

    lines.push(Line::from(""));
    lines.push(sec("spawn form  (c)"));
    lines.push(row("tab", "switch between cwd and flags fields"));
    lines.push(row("↑ / ↓  (cwd field)", "cycle recent directories"));
    lines.push(row("Ctrl-Y", "toggle --dangerously-skip-permissions"));
    lines.push(row("enter", "create"));
    lines.push(row("esc", "cancel"));

    lines.push(Line::from(""));
    lines.push(sec("from the shell  (outside the TUI)"));
    lines.push(row("claws", "open the dashboard"));
    lines.push(row("claws update", "install the latest release in place"));
    lines.push(row("claws kill-server", "stop the daemon and all sessions"));
    lines.push(row("claws logs", "print the log file path"));
    lines.push(row("claws --version", "print version"));

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  press any key to close",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));

    f.render_widget(
        Paragraph::new(lines),
        Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), inner.height),
    );
}

fn draw_rename(f: &mut ratatui::Frame, input: &str, cursor: usize) {
    let parent = f.area();
    let w = 60.min(parent.width.saturating_sub(4));
    let h = 5;
    let area = centered_rect(w, h, parent);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(ratatui::text::Span::styled(
            " rename ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let row = Rect::new(inner.x + 1, inner.y + 1, inner.width.saturating_sub(2), 1);
    f.render_widget(
        Paragraph::new(input.to_string()).style(Style::default().fg(Color::White)),
        row,
    );
    f.set_cursor_position(Position::new(row.x + cursor as u16, row.y));
    f.render_widget(
        Paragraph::new(" enter save  ·  esc cancel  ·  blank reverts to ai_title")
            .style(Style::default().fg(Color::DarkGray)),
        Rect::new(inner.x + 1, inner.y + 2, inner.width.saturating_sub(2), 1),
    );
}

fn draw_details(f: &mut ratatui::Frame, s: &SessionInfo) {
    let parent = f.area();
    let w = 80.min(parent.width.saturating_sub(4));
    let h = 18.min(parent.height.saturating_sub(2));
    let area = centered_rect(w, h, parent);
    f.render_widget(Clear, area);
    let color = status_color(&s.status);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(ratatui::text::Span::styled(
            format!(" details — {} ", s.id),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    use ratatui::text::{Line, Span};
    let label = Style::default().fg(Color::DarkGray);
    let val = Style::default().fg(Color::White);
    let mut lines: Vec<Line> = Vec::new();
    let row = |k: &'static str, v: String| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {:<14}", k), label),
            Span::styled(v, val),
        ])
    };

    let title = s
        .display_override
        .clone()
        .or_else(|| s.ai_title.clone())
        .unwrap_or_else(|| s.name.clone());
    lines.push(row("title", title));
    lines.push(row("name", s.name.clone()));
    lines.push(row("status", s.status.clone()));
    if let Some(c) = s.exit_code {
        lines.push(row("exit code", c.to_string()));
    }
    lines.push(row("cwd", s.cwd.clone()));
    if let Some(m) = &s.model {
        lines.push(row("model", m.clone()));
    }
    if let Some(t) = &s.current_tool {
        lines.push(row("running tool", t.clone()));
    }
    lines.push(row("turns", s.turn_count.to_string()));
    lines.push(row(
        "tokens",
        format!(
            "input {}  ·  output {}  ·  cache {}",
            compact_num(s.tokens_input),
            compact_num(s.tokens_output),
            compact_num(s.tokens_cache_read)
        ),
    ));
    lines.push(row("started", format_time_ago(s.started_at_ms)));
    lines.push(row("last seen", format_time_ago(s.last_activity_ms)));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  last message",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        format!("  {}", s.last_message.as_deref().unwrap_or("(none)")),
        Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  press any key to close",
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
    )));
    let _ = color;

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        Rect::new(inner.x, inner.y, inner.width, inner.height),
    );
}

fn draw_spawn_form(
    f: &mut ratatui::Frame,
    cwd: &str,
    cwd_cursor: usize,
    args: &str,
    args_cursor: usize,
    focus: FormField,
    recent_selected: usize,
    recent: &[String],
) {
    let parent = f.area();
    let w = 80.min(parent.width.saturating_sub(4));
    let recent_rows = recent.len().min(6) as u16;
    // 9 fixed rows (label/input/gap/label/input/examples/sep/label/help) +
    // recent_rows + 2 for borders. Previously omitted the +2, so ratatui
    // collapsed the args-input row to zero height — typed text was rendering
    // behind the examples line.
    let h = 11 + recent_rows;
    let area = centered_rect(w, h, parent);

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(ratatui::text::Span::styled(
            " spawn session ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // cwd label
            Constraint::Length(1), // cwd input
            Constraint::Length(1), // gap
            Constraint::Length(1), // args label
            Constraint::Length(1), // args input
            Constraint::Length(1), // args examples
            Constraint::Length(1), // separator
            Constraint::Length(1), // recent label
            Constraint::Min(1),    // recent list
            Constraint::Length(1), // help
        ])
        .split(Rect::new(
            inner.x + 1,
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        ));

    let label_dim = Style::default().fg(Color::DarkGray);
    let label_active = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let working_dir_label = if focus == FormField::Cwd { "working directory" } else { "working directory" };
    let flags_label = if focus == FormField::Args { "claude flags" } else { "claude flags" };

    f.render_widget(
        Paragraph::new(working_dir_label)
            .style(if focus == FormField::Cwd { label_active } else { label_dim }),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(cwd.to_string()).style(Style::default().fg(Color::White)),
        chunks[1],
    );
    // Verify-by-eye that the args row is actually being drawn: leave a mini
    // status crumb on the args label that shows the current arg-string length.
    // (Useful for diagnosing "I typed but I see nothing"; can be removed once
    // confirmed working.)
    let args_crumb = format!("  ({} chars)", args.chars().count());
    let crumb_w = args_crumb.chars().count() as u16;
    let crumb_x = chunks[3].x + chunks[3].width.saturating_sub(crumb_w);
    f.render_widget(
        Paragraph::new(args_crumb).style(Style::default().fg(Color::DarkGray)),
        Rect::new(crumb_x, chunks[3].y, crumb_w, 1),
    );

    f.render_widget(
        Paragraph::new(flags_label)
            .style(if focus == FormField::Args { label_active } else { label_dim }),
        chunks[3],
    );

    // Always render the args text — no conditional placeholder. Putting a
    // visible "▎ " gutter at the start of the row makes the field obvious
    // even when empty, and `replace_all` ratatui's Buffer with a fresh
    // styled Paragraph means there's no stale-cell bleed-through from a
    // prior frame's placeholder string.
    let args_display = if args.is_empty() {
        if focus == FormField::Args {
            String::new() // cursor-only; the bar marker still shows below
        } else {
            "(empty — claude runs with default flags)".to_string()
        }
    } else {
        args.to_string()
    };
    let args_style = if args.is_empty() && focus != FormField::Args {
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(Color::White)
    };
    f.render_widget(
        Paragraph::new(args_display).style(args_style),
        chunks[4],
    );
    f.render_widget(
        Paragraph::new("e.g.  --dangerously-skip-permissions   --system-prompt \"…\"   --effort xhigh   --add-dir <path>")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[5],
    );

    match focus {
        FormField::Cwd => {
            f.set_cursor_position(Position::new(
                chunks[1].x + cwd_cursor as u16,
                chunks[1].y,
            ));
        }
        FormField::Args => {
            f.set_cursor_position(Position::new(
                chunks[4].x + args_cursor as u16,
                chunks[4].y,
            ));
        }
    }

    f.render_widget(
        Paragraph::new("─".repeat(chunks[6].width as usize))
            .style(Style::default().fg(Color::DarkGray)),
        chunks[6],
    );
    f.render_widget(
        Paragraph::new("recent directories").style(Style::default().fg(Color::DarkGray)),
        chunks[7],
    );

    for (i, dir) in recent.iter().take(6).enumerate() {
        let y = chunks[8].y + i as u16;
        if y >= chunks[8].y + chunks[8].height {
            break;
        }
        let marker = if i == recent_selected && focus == FormField::Cwd {
            "› "
        } else {
            "  "
        };
        let style = if i == recent_selected && focus == FormField::Cwd {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let avail = chunks[8].width.saturating_sub(2) as usize;
        let line = format!("{marker}{}", truncate_ellipsis(&shorten_home(dir), avail));
        f.render_widget(
            Paragraph::new(line).style(style),
            Rect::new(chunks[8].x, y, chunks[8].width, 1),
        );
    }

    f.render_widget(
        Paragraph::new(" enter create   ·   tab switch field   ·   ↑/↓ recent (cwd field)   ·   esc cancel")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[9],
    );
}

fn centered_rect(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

fn draw_dashboard(f: &mut ratatui::Frame, app: &App) {
    let theme = crate::theme::current();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // title + separator
            Constraint::Min(0),    // body
            Constraint::Length(2), // separator + footer
        ])
        .split(f.area());

    // Title row: theme-accent "claws" + dim count + right-aligned cwd hint
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
    use ratatui::text::{Line, Span};
    let mut spans = vec![
        Span::styled(
            " claws ",
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(theme.dim)),
        Span::styled(count_part.clone(), Style::default().fg(theme.body)),
    ];
    if !cwd_hint.is_empty() {
        let used = " claws ".chars().count() + " · ".chars().count() + count_part.chars().count();
        let avail = title_w.saturating_sub(used + 2);
        let cwd_short = truncate_ellipsis(cwd_hint, avail);
        let pad = title_w
            .saturating_sub(used + cwd_short.chars().count())
            .saturating_sub(1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(format!("{cwd_short} "), Style::default().fg(theme.cwd)));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(chunks[0].x, chunks[0].y, chunks[0].width, 1),
    );
    f.render_widget(
        Paragraph::new("─".repeat(title_w)).style(Style::default().fg(theme.dim)),
        Rect::new(chunks[0].x, chunks[0].y + 1, chunks[0].width, 1),
    );

    let visible = app.visible_sessions();
    if visible.is_empty() {
        if app.filter.is_some() {
            f.render_widget(
                Paragraph::new("\n\n  no sessions match the filter\n  press esc to clear")
                    .style(Style::default().fg(Color::DarkGray)),
                chunks[1],
            );
        } else {
            draw_empty_state(f, chunks[1]);
        }
    } else {
        let owned: Vec<SessionInfo> = visible.iter().map(|s| (*s).clone()).collect();
        draw_split(f, &owned, app.selected, app.tick_phase, chunks[1], app.detail_scroll);
        // Sidebar is single-column for navigation purposes.
        let app_ptr = app as *const App as *mut App;
        unsafe { (*app_ptr).grid_cols = 1; }
    }

    // Footer separator + help/status line
    let footer_w = chunks[2].width as usize;
    f.render_widget(
        Paragraph::new("─".repeat(footer_w)).style(Style::default().fg(theme.dim)),
        Rect::new(chunks[2].x, chunks[2].y, chunks[2].width, 1),
    );
    let status_text = if let Some(filter) = app.filter.as_deref() {
        format!(" / {filter}_  (esc clear · enter exit input)")
    } else if let Some((msg, _)) = &app.status_message {
        format!(" {msg}")
    } else {
        " hjkl  move    enter  attach    c  new    r  rename    R  restart    x  close    /  filter    i  info    t  theme    ?  help    q  quit"
            .to_string()
    };
    f.render_widget(
        Paragraph::new(status_text).style(Style::default().fg(theme.dim)),
        Rect::new(chunks[2].x, chunks[2].y + 1, chunks[2].width, 1),
    );
}

fn draw_empty_state(f: &mut ratatui::Frame, area: Rect) {
    let theme = crate::theme::current();

    let crab = [
        "      ▟▙       ▟▙       ",
        "      ██▄▄▄▄▄▄▄██       ",
        "    ▟████ ◉   ◉ ████▙   ",
        "  ▟████████▀▀▀████████▙ ",
        "  ▔▀██▀▔ ▀▀▀▀▀▀▀ ▔▀██▔▀ ",
        "    ▘▝     ▔▔▔     ▝▘   ",
    ];
    // figlet "Standard" output. Five rows, the W is two clean V-joins.
    let wordmark = [
        "       _                     ",
        "   ___| | __ ___      _____  ",
        "  / __| |/ _` \\ \\ /\\ / / __| ",
        " | (__| | (_| |\\ V  V /\\__ \\ ",
        "  \\___|_|\\__,_| \\_/\\_/ |___/ ",
    ];
    let tagline = "many claws, one terminal";
    let prompt = "press  c  to spawn a session   ·   ?  for help";

    // Total block height: crab + 1 gap + wordmark + 1 gap + tagline + 1 gap + prompt.
    let total_h = (crab.len() + 1 + wordmark.len() + 1 + 1 + 1 + 1) as u16;
    let block_w = wordmark[0].chars().count().max(crab[0].chars().count()) as u16;

    let y0 = area.y + area.height.saturating_sub(total_h) / 2;
    let x0 = area.x + area.width.saturating_sub(block_w) / 2;

    let mut row = 0u16;
    for line in &crab {
        let w = line.chars().count() as u16;
        let x = x0 + (block_w.saturating_sub(w)) / 2;
        f.render_widget(
            Paragraph::new(line.to_string())
                .style(Style::default().fg(theme.idle)),
            Rect::new(x, y0 + row, w, 1),
        );
        row += 1;
    }
    row += 1;

    for line in &wordmark {
        let w = line.chars().count() as u16;
        let x = x0 + (block_w.saturating_sub(w)) / 2;
        f.render_widget(
            Paragraph::new(line.to_string())
                .style(Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
            Rect::new(x, y0 + row, w, 1),
        );
        row += 1;
    }
    row += 1;

    let tag_w = tagline.chars().count() as u16;
    let tag_x = x0 + (block_w.saturating_sub(tag_w)) / 2;
    f.render_widget(
        Paragraph::new(tagline.to_string())
            .style(Style::default().fg(theme.title_fallback).add_modifier(Modifier::ITALIC)),
        Rect::new(tag_x, y0 + row, tag_w, 1),
    );
    row += 2;

    let prompt_w = prompt.chars().count() as u16;
    let prompt_x = x0 + (block_w.saturating_sub(prompt_w)) / 2;
    f.render_widget(
        Paragraph::new(prompt.to_string()).style(Style::default().fg(theme.dim)),
        Rect::new(prompt_x, y0 + row, prompt_w, 1),
    );
}

// Sidebar (left) / detail (right) split layout.
const SIDEBAR_W: u16 = 30;
const SIDEBAR_ROW_H: u16 = 3;

fn draw_split(
    f: &mut ratatui::Frame,
    sessions: &[SessionInfo],
    selected: usize,
    tick_phase: u32,
    area: Rect,
    detail_scroll: u16,
) {
    let theme = crate::theme::current();
    if area.width < SIDEBAR_W + 30 {
        // Terminal too narrow for split — fall back to a stacked single column.
        draw_sidebar(f, sessions, selected, tick_phase, area);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(SIDEBAR_W),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);

    draw_sidebar(f, sessions, selected, tick_phase, chunks[0]);
    // Vertical separator in theme color
    for y in chunks[1].y..chunks[1].y + chunks[1].height {
        f.render_widget(
            Paragraph::new("│").style(Style::default().fg(theme.dim)),
            Rect::new(chunks[1].x, y, 1, 1),
        );
    }
    if let Some(s) = sessions.get(selected) {
        draw_detail(f, s, tick_phase, chunks[2], detail_scroll);
    }
}

fn draw_sidebar(
    f: &mut ratatui::Frame,
    sessions: &[SessionInfo],
    selected: usize,
    tick_phase: u32,
    area: Rect,
) {
    let stride = SIDEBAR_ROW_H;
    let max_visible = ((area.height + 1) / stride) as usize;
    let scroll_off = selected.saturating_sub(max_visible.saturating_sub(1).max(0));

    for (idx, s) in sessions.iter().enumerate() {
        if idx < scroll_off {
            continue;
        }
        let render_row = (idx - scroll_off) as u16;
        let y = area.y + render_row * stride;
        if y + 2 > area.y + area.height {
            break;
        }
        draw_sidebar_entry(
            f,
            s,
            idx == selected,
            tick_phase,
            Rect::new(area.x, y, area.width, 2),
        );
    }
}

fn draw_sidebar_entry(
    f: &mut ratatui::Frame,
    s: &SessionInfo,
    selected: bool,
    tick_phase: u32,
    area: Rect,
) {
    let theme = crate::theme::current();
    let color = status_color_pulsed(&s.status, tick_phase);
    let glyph = status_glyph(&s.status, tick_phase);

    // Selected entry gets a 1-col theme-accent gutter on the left + subtle bg
    // tint across the whole row. Much more visible than the previous ▎ marker.
    if selected {
        for r in 0..2u16 {
            f.render_widget(
                Paragraph::new(" ").style(Style::default().bg(theme.accent)),
                Rect::new(area.x, area.y + r, 1, 1),
            );
        }
    }

    let (title, title_is_auto) = match s.display_override.as_deref().or(s.ai_title.as_deref()) {
        Some(t) if !t.is_empty() => (t.to_string(), true),
        _ => (s.name.clone(), false),
    };
    let title_style = if title_is_auto {
        Style::default().fg(theme.title).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.title_fallback).add_modifier(Modifier::ITALIC)
    };

    use ratatui::text::{Line, Span};

    // Row 1: glyph + title
    let title_avail = area.width.saturating_sub(4) as usize;
    let title_truncated = truncate_ellipsis(&title, title_avail);
    let title_line = Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{glyph} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(title_truncated, title_style),
    ]);
    f.render_widget(
        Paragraph::new(title_line),
        Rect::new(area.x + 1, area.y, area.width.saturating_sub(1), 1),
    );

    // Row 2 (indented): status · time-ago
    let status_label = display_status(&s.status);
    let time_ago = format_time_ago(s.last_activity_ms);
    let exit_part = s.exit_code.map(|c| format!(" · exit {c}")).unwrap_or_default();
    let meta_line = Line::from(vec![
        Span::raw("    "),
        Span::styled(status_label, Style::default().fg(color)),
        Span::styled(format!(" · {time_ago}"), Style::default().fg(theme.dim)),
        Span::styled(exit_part, Style::default().fg(theme.dim)),
    ]);
    f.render_widget(
        Paragraph::new(meta_line),
        Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(1), 1),
    );
}

fn draw_detail(
    f: &mut ratatui::Frame,
    s: &SessionInfo,
    tick_phase: u32,
    area: Rect,
    detail_scroll: u16,
) {
    let color = status_color_pulsed(&s.status, tick_phase);
    let glyph = status_glyph(&s.status, tick_phase);
    let (title, title_is_auto) = match s.display_override.as_deref().or(s.ai_title.as_deref()) {
        Some(t) if !t.is_empty() => (t.to_string(), true),
        _ => (s.name.clone(), false),
    };
    let title_style = if title_is_auto {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC)
    };

    use ratatui::text::{Line, Span};

    let pad = Rect::new(
        area.x + 2,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );

    // Header: glyph  title  ·  status  ·  uptime  ·  time-ago  ·  model
    let status_label = display_status(&s.status);
    let uptime = format_uptime(s.started_at_ms);
    let time_ago = format_time_ago(s.last_activity_ms);
    let model = s.model.as_deref().map(short_model).unwrap_or("—");
    let header_line = Line::from(vec![
        Span::styled(format!("{glyph}  "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(title, title_style),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(status_label, Style::default().fg(color)),
        Span::styled(format!("  ·  up {uptime}"), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("  ·  {time_ago}"), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("  ·  {model}"), Style::default().fg(Color::Magenta)),
    ]);
    f.render_widget(Paragraph::new(header_line), Rect::new(pad.x, pad.y, pad.width, 1));

    // Separator
    f.render_widget(
        Paragraph::new("─".repeat(pad.width as usize))
            .style(Style::default().fg(Color::DarkGray)),
        Rect::new(pad.x, pad.y + 1, pad.width, 1),
    );

    let cost = s.model.as_deref().map(|m| estimate_cost(m, s)).unwrap_or(0.0);
    let cwd = shorten_path_left(&shorten_home(&s.cwd), pad.width.saturating_sub(12) as usize);
    let label = Style::default().fg(Color::DarkGray);
    let val = Style::default().fg(Color::White);
    let row = |k: &'static str, v: String, vstyle: Style| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{:<10}", k), label),
            Span::styled(v, vstyle),
        ])
    };

    let mut info_lines: Vec<Line> = Vec::new();
    info_lines.push(row("cwd", cwd, Style::default().fg(Color::Blue)));
    info_lines.push(row("turns", s.turn_count.to_string(), val));
    if let Some(t) = &s.current_tool {
        info_lines.push(row(
            "tool",
            format!("⚙ {}", truncate_ellipsis(t, pad.width.saturating_sub(14) as usize)),
            Style::default().fg(Color::Cyan),
        ));
    }
    if let Some(pct) = s.context_pct {
        let bar = context_bar(pct, 20);
        let used = s.context_used.as_deref().unwrap_or("?");
        let total = s.context_total.as_deref().unwrap_or("?");
        let bar_color = match pct {
            0..=59 => Color::Green,
            60..=84 => Color::Yellow,
            _ => Color::Red,
        };
        info_lines.push(Line::from(vec![
            Span::styled(format!("{:<10}", "context"), label),
            Span::styled(bar, Style::default().fg(bar_color)),
            Span::styled(
                format!("  {pct}%  ·  {used}/{total}"),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    } else if matches!(s.status.as_str(), "idle" | "streaming" | "awaiting_permission") {
        // Reaches here only when BOTH paths failed: status-bar scrape didn't
        // match and we don't have token usage to compute from. Most often
        // this is a fresh session before its first response.
        info_lines.push(Line::from(vec![
            Span::styled(format!("{:<10}", "context"), label),
            Span::styled(
                "(appears after first response)",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
    let info_h = info_lines.len() as u16;
    f.render_widget(
        Paragraph::new(info_lines),
        Rect::new(pad.x, pad.y + 3, pad.width, info_h),
    );

    // Last message section
    let msg_y = pad.y + 3 + info_h + 1;
    if msg_y < pad.y + pad.height {
        f.render_widget(
            Paragraph::new("last message").style(label),
            Rect::new(pad.x, msg_y, pad.width, 1),
        );
        let msg_text = match s.last_message.as_deref() {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => "(no messages yet)".to_string(),
        };
        let msg_style = if s.last_message.is_some() {
            Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC)
        } else {
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)
        };
        // Use all rows between the label and the bottom-anchored stats line,
        // minus one for a visual breathing gap. The stats render after this so
        // any wrap-overflow into the last row is overwritten cleanly.
        let msg_top = msg_y + 1;
        let bottom = pad.y + pad.height;
        let msg_h = bottom.saturating_sub(msg_top + 2).max(1);
        if msg_top < bottom {
            f.render_widget(
                Paragraph::new(format!("  {msg_text}"))
                    .style(msg_style)
                    .wrap(Wrap { trim: false })
                    .scroll((detail_scroll, 0)),
                Rect::new(pad.x, msg_top, pad.width, msg_h),
            );
        }
    }

    // Stats — anchor to bottom of detail pane
    let stats_line = Line::from(vec![
        Span::styled(
            format!("{} → {}", compact_num(s.tokens_input), compact_num(s.tokens_output)),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            format!("  ·  cache {}", compact_num(s.tokens_cache_read)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("  ·  {}", format_cost(cost)),
            Style::default().fg(Color::Green),
        ),
    ]);
    if pad.height >= 2 {
        let stats_y = pad.y + pad.height - 1;
        f.render_widget(
            Paragraph::new(stats_line),
            Rect::new(pad.x, stats_y, pad.width, 1),
        );
    }
}

fn status_color(status: &str) -> Color {
    status_color_pulsed(status, 0)
}

fn status_color_pulsed(status: &str, tick_phase: u32) -> Color {
    let t = crate::theme::current();
    match status {
        "spawning" => t.spawning,
        "idle" => t.idle,
        "streaming" => t.working,
        "awaiting_permission" => {
            if (tick_phase / 5) % 2 == 0 {
                t.awaiting_a
            } else {
                t.awaiting_b
            }
        }
        "exited" => t.exited,
        _ => t.title,
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
    let color = status_color_pulsed(&s.status, tick_phase);
    let border_type = if selected { BorderType::Double } else { BorderType::Plain };
    let border_style = if selected {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color)
    };

    let glyph = status_glyph(&s.status, tick_phase);
    let (title_label, title_is_auto) = match s.display_override.as_deref().or(s.ai_title.as_deref()) {
        Some(t) if !t.is_empty() => (t.to_string(), true),
        _ => (s.name.clone(), false),
    };
    let title_style = if title_is_auto {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC)
    };

    use ratatui::text::{Line, Span};

    let status_label = display_status(&s.status);
    let uptime = format_uptime(s.started_at_ms);
    let time_ago = format_time_ago(s.last_activity_ms);
    let model = s.model.as_deref().map(short_model).unwrap_or("—");

    // Title spans the top border. Pack the most important identifying
    // info in here — name, status, uptime, last-active, model — so the
    // card body can focus on session content (last message + stats).
    let title_line = Line::from(vec![
        Span::styled(format!(" {glyph}  "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(title_label, title_style),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(status_label, Style::default().fg(color)),
        Span::styled(format!("  ·  up {uptime}"), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("  ·  {time_ago}"), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("  ·  {model} "), Style::default().fg(Color::Magenta)),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(border_type)
        .border_style(border_style)
        .title(title_line);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Inner is 5 rows high (STACKED_CARD_H=7 minus 2 borders). Lay out as:
    //   row 0: top padding (blank)
    //   row 1: cwd (blue)
    //   row 2: last message (italic gray, full-width)
    //   row 3: stats line (turns / tokens / cost / current_tool / exit)
    //   row 4: bottom padding (blank)
    // 2-col left padding inside.
    let body = Rect::new(
        inner.x + 2,
        inner.y,
        inner.width.saturating_sub(4),
        inner.height,
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top pad
            Constraint::Length(1), // cwd
            Constraint::Length(1), // last message
            Constraint::Length(1), // stats
            Constraint::Length(1), // bottom pad
        ])
        .split(body);

    let cwd_short = shorten_path_left(&shorten_home(&s.cwd), chunks[1].width as usize);
    f.render_widget(
        Paragraph::new(cwd_short).style(Style::default().fg(Color::Blue)),
        chunks[1],
    );

    let preview = match s.last_message.as_deref() {
        Some(m) if !m.is_empty() => truncate_ellipsis(m, chunks[2].width as usize),
        _ => "(no messages yet)".to_string(),
    };
    let preview_style = if s.last_message.is_some() {
        Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)
    };
    f.render_widget(
        Paragraph::new(preview).style(preview_style),
        chunks[2],
    );

    let cost = s.model.as_deref().map(|m| estimate_cost(m, s)).unwrap_or(0.0);
    let tool_part = s
        .current_tool
        .as_deref()
        .map(|t| format!("  ·  ⚙ {}", truncate_ellipsis(t, 24)))
        .unwrap_or_default();
    let exit_part = s.exit_code.map(|c| format!("  ·  exit {c}")).unwrap_or_default();
    let stats_line = Line::from(vec![
        Span::styled(format!("{}t", s.turn_count), Style::default().fg(Color::Gray)),
        Span::styled(
            format!("  ·  {} → {}", compact_num(s.tokens_input), compact_num(s.tokens_output)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("  ·  cache {}", compact_num(s.tokens_cache_read)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("  ·  {}", format_cost(cost)),
            Style::default().fg(Color::Green),
        ),
        Span::styled(tool_part, Style::default().fg(Color::Cyan)),
        Span::styled(exit_part, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(stats_line), chunks[3]);
}

fn shorten_home(path: &str) -> String {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    if home.is_empty() {
        return path.to_string();
    }
    let path_l = path.to_ascii_lowercase().replace('\\', "/");
    let home_l = home.to_ascii_lowercase().replace('\\', "/");
    if path_l.starts_with(&home_l) && path.len() >= home.len() {
        let rest = &path[home.len()..];
        return format!("~{rest}");
    }
    path.to_string()
}

fn shorten_path_left(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let take = max - 1;
    let skipped = chars.len() - take;
    let mut out = String::with_capacity(max);
    out.push('…');
    out.extend(chars.into_iter().skip(skipped));
    out
}

fn format_uptime(started_at_ms: u128) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let elapsed_ms = now.saturating_sub(started_at_ms);
    let s = elapsed_ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Rough USD estimate from token counts. Pricing per 1M tokens, snapshot
/// of public list prices — adjust as Anthropic updates.
fn estimate_cost(model: &str, info: &SessionInfo) -> f64 {
    let (in_rate, out_rate, cache_rate) = if model.contains("opus") {
        (15.0, 75.0, 1.50)
    } else if model.contains("sonnet") {
        (3.0, 15.0, 0.30)
    } else if model.contains("haiku") {
        (0.80, 4.0, 0.08)
    } else {
        (0.0, 0.0, 0.0)
    };
    let inp = info.tokens_input as f64;
    let outp = info.tokens_output as f64;
    let cache = info.tokens_cache_read as f64;
    (inp * in_rate + outp * out_rate + cache * cache_rate) / 1_000_000.0
}

fn format_cost(c: f64) -> String {
    format!("${c:.2}")
}

/// Render a 1-row unicode bar of `width` cells filled to `pct`%.
fn context_bar(pct: u8, width: usize) -> String {
    let pct = pct.min(100) as usize;
    let filled = (pct * width + 50) / 100;
    let empty = width.saturating_sub(filled);
    let mut s = String::with_capacity(width * 3);
    for _ in 0..filled {
        s.push('█');
    }
    for _ in 0..empty {
        s.push('░');
    }
    s
}

fn display_status(s: &str) -> &'static str {
    match s {
        "spawning" => "spawning",
        "idle" => "idle",
        "streaming" => "working",
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

    let (session_id, parser, prefix_active, scroll) = match &app.view {
        View::Attached {
            session_id,
            parser,
            prefix_active,
            scroll,
            ..
        } => (*session_id, parser, *prefix_active, scroll.as_ref()),
        _ => return,
    };

    // Header — name, status with color, time-ago, model
    let info = app.sessions.iter().find(|s| s.id == session_id);
    use ratatui::text::{Line, Span};
    let header_line = if let Some(s) = info {
        let color = status_color(&s.status);
        let glyph = status_glyph(&s.status, app.tick_phase);
        let (name, name_is_auto) = match s.ai_title.as_deref() {
            Some(t) if !t.is_empty() => (t.to_string(), true),
            _ => (s.name.clone(), false),
        };
        let name_style = if name_is_auto {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC)
        };
        let model = s.model.as_deref().map(short_model).unwrap_or("");
        let time_ago = format_time_ago(s.last_activity_ms);
        Line::from(vec![
            Span::styled(format!(" {glyph}  "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(name, name_style),
            Span::styled(format!("  ·  {}", display_status(&s.status)), Style::default().fg(color)),
            Span::styled(format!("  ·  {time_ago}"), Style::default().fg(Color::Gray)),
            Span::styled(format!("  ·  {model}"), Style::default().fg(Color::Magenta)),
        ])
    } else {
        Line::from(Span::styled(format!(" attached — {session_id} "), Style::default().fg(Color::Cyan)))
    };
    f.render_widget(Paragraph::new(header_line), chunks[0]);

    render_pty(f, parser, scroll, chunks[1]);

    let footer = if let Some(s) = scroll {
        let pos = if s.bytes.is_empty() {
            0
        } else {
            s.offset.saturating_mul(100) / s.bytes.len()
        };
        format!(
            " ◀ scroll  ·  {pos}%  ·  ↑↓ jk  ·  pgup/pgdn  ·  g/G top/bottom  ·  esc resume live"
        )
    } else if prefix_active {
        " ◆ prefix:  d  detach   n/p  next/prev   1-9  jump   [  scroll   q  quit".to_string()
    } else {
        " Ctrl-Space  prefix (d=detach n/p=cycle 1-9=jump [=scroll)    keys → claude".to_string()
    };
    let footer_color = if scroll.is_some() {
        Color::Magenta
    } else if prefix_active {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(footer_color)),
        chunks[2],
    );
}

fn render_pty(
    f: &mut ratatui::Frame,
    parser: &vt100::Parser,
    scroll: Option<&ScrollState>,
    area: Rect,
) {
    // In scroll mode, build an ephemeral parser fed bytes [0..offset] of the
    // captured ring buffer and render its screen. Otherwise render the live
    // parser. The ephemeral binding has to outlive the screen reference.
    let ephemeral = scroll.map(|s| {
        let (rows, cols) = parser.screen().size();
        let mut p = vt100::Parser::new(rows, cols, 0);
        let end = s.offset.min(s.bytes.len());
        p.process(&s.bytes[..end]);
        p
    });
    let screen = match &ephemeral {
        Some(p) => p.screen(),
        None => parser.screen(),
    };
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
