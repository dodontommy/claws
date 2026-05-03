use crate::client;
use crate::protocol::SessionInfo;
use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
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

// Sessions spawned with this flag get a red `!` indicator in the sidebar so
// the danger is visible at a glance — bypassing permission prompts means
// claude can run any tool without asking.
const DANGEROUS_FLAG: &str = "--dangerously-skip-permissions";


pub async fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // Deliberately no EnableMouseCapture: capturing mouse events disables
    // the terminal's native drag-to-select / copy-paste, which is the more
    // valuable interaction. Keyboard nav handles everything mouse used to.
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_inner(&mut terminal).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), DisableBracketedPaste, LeaveAlternateScreen)?;
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
    Worktrees,
    Model,
}

/// Pill choices for the spawn-form model selector. `Default` means "don't
/// pass --model" — claude picks. The named variants map to claude's short
/// model aliases (it also accepts full IDs, but the alias surface is the
/// useful one for a one-keystroke picker).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ModelChoice {
    Default,
    Opus,
    Sonnet,
    Haiku,
}

impl ModelChoice {
    const ALL: [ModelChoice; 4] = [
        ModelChoice::Default,
        ModelChoice::Opus,
        ModelChoice::Sonnet,
        ModelChoice::Haiku,
    ];
    fn label(&self) -> &'static str {
        match self {
            ModelChoice::Default => "default",
            ModelChoice::Opus => "opus",
            ModelChoice::Sonnet => "sonnet",
            ModelChoice::Haiku => "haiku",
        }
    }
    /// What we pass to `claude --model`. `None` means don't pass the flag.
    fn as_arg(&self) -> Option<&'static str> {
        match self {
            ModelChoice::Default => None,
            ModelChoice::Opus => Some("opus"),
            ModelChoice::Sonnet => Some("sonnet"),
            ModelChoice::Haiku => Some("haiku"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WtFormField {
    Branch,
    Path,
}

enum Modal {
    SpawnForm {
        cwd: String,
        cwd_cursor: usize,
        args: String,
        args_cursor: usize,
        focus: FormField,
        recent_selected: usize,
        // Worktree-aware spawn. Recomputed on every cwd change.
        repo_root: Option<PathBuf>,
        worktrees: Vec<crate::git::Worktree>,
        collision: bool,
        /// 0 = "[+ new worktree]" row; 1..=worktrees.len() = worktrees[idx-1].
        wt_selected: usize,
        last_scan_cwd: String,
        /// Suffix to append to `cwd` to complete it to an existing directory.
        /// Rendered as dim ghost text after the cursor. None when the typed
        /// path is already complete or has no unique completion candidate.
        cwd_completion: Option<String>,
        /// True when `cwd` is non-empty and doesn't currently resolve to a
        /// directory. We surface a "[Enter to create]" hint and mkdir -p on
        /// submit instead of erroring.
        cwd_missing: bool,
        model: ModelChoice,
    },
    WorktreeNew {
        repo_root: PathBuf,
        branch: String,
        branch_cursor: usize,
        path: String,
        path_cursor: usize,
        focus: WtFormField,
        /// Last error from `git worktree add`, shown inline. Cleared on edit.
        error: Option<String>,
    },
    Rename {
        session_id: Uuid,
        input: String,
        cursor: usize,
    },
    Details {
        session_id: Uuid,
    },
    Help {
        scroll: u16,
    },
    ThemePicker {
        selected_idx: usize,
        // Theme name active when the picker opened. Esc reverts to this.
        original_name: String,
    },
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
    grid_mode: bool,
    filter: Option<String>,
    filter_cursor: usize,
    /// Vertical scroll offset for the detail pane's last-message paragraph.
    /// Reset to 0 whenever the selected session changes.
    detail_scroll: u16,
    /// Per-session monotonically increasing "last status seen" timestamps.
    /// Used to flash a row briefly when the daemon reports a status change.
    last_status: std::collections::HashMap<Uuid, (String, SystemTime)>,
    /// Cached `git rev-parse --abbrev-ref HEAD` per session cwd. Keyed by
    /// session id and computed once per session lifetime — branches inside
    /// a session can change, but we re-derive on F5 / daemon restart and
    /// most worktrees stay on one branch for the duration of a session.
    session_branches: std::collections::HashMap<Uuid, Option<String>>,
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
            grid_mode: false,
            filter: None,
            filter_cursor: 0,
            detail_scroll: 0,
            last_status: std::collections::HashMap::new(),
            session_branches: std::collections::HashMap::new(),
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
                    Some(Ok(Event::Paste(s))) => {
                        // The terminal grouped a multi-line paste for us via
                        // bracketed-paste mode. When attached, wrap the
                        // payload in `\x1b[200~ ... \x1b[201~` so Claude
                        // (which has its own bracketed-paste handling) sees
                        // it as one block and doesn't submit on every \n.
                        if let View::Attached { session_id, .. } = &app.view {
                            let mut bytes = Vec::with_capacity(s.len() + 12);
                            bytes.extend_from_slice(b"\x1b[200~");
                            bytes.extend_from_slice(s.as_bytes());
                            bytes.extend_from_slice(b"\x1b[201~");
                            let _ = client::send_input_raw(*session_id, bytes).await;
                        }
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
            // Sort: needs-you first, then streaming, then idle, spawning, exited last.
            // Within active buckets sort by id (spawn order) so concurrently-streaming
            // rows don't swap places every tick. Within quiet buckets, sort by recent
            // activity so the most recently touched idle session bubbles up.
            list.sort_by(|a, b| {
                let pa = sort_priority(&a.status);
                let pb = sort_priority(&b.status);
                pa.cmp(&pb).then_with(|| {
                    if is_active_bucket(&a.status) {
                        a.id.cmp(&b.id)
                    } else {
                        b.last_activity_ms.cmp(&a.last_activity_ms)
                    }
                })
            });
            // Track per-session status transitions so the sidebar can flash
            // a row briefly when the daemon reports a state change.
            let now = SystemTime::now();
            for s in &list {
                match app.last_status.get(&s.id) {
                    None => {
                        // First sight — record without flashing.
                        app.last_status.insert(s.id, (s.status.clone(), UNIX_EPOCH));
                    }
                    Some((prev, _)) if *prev != s.status => {
                        app.last_status.insert(s.id, (s.status.clone(), now));
                    }
                    _ => {}
                }
            }
            app.last_status.retain(|id, _| list.iter().any(|s| s.id == *id));
            // Branch cache: populate on first sight, drop on disappearance.
            for s in &list {
                if !app.session_branches.contains_key(&s.id) {
                    let cwd = std::path::Path::new(&s.cwd);
                    let branch = if cwd.is_dir() {
                        crate::git::current_branch(cwd)
                    } else {
                        None
                    };
                    app.session_branches.insert(s.id, branch);
                }
            }
            app.session_branches.retain(|id, _| list.iter().any(|s| s.id == *id));
            app.sessions = list;
            if !app.sessions.is_empty() && app.selected >= app.sessions.len() {
                app.selected = app.sessions.len() - 1;
            }
        }
        Err(e) => app.set_status(format!("daemon error: {e}")),
    }
}

fn is_active_bucket(status: &str) -> bool {
    matches!(status, "awaiting_permission" | "streaming" | "spawning")
}

fn sort_priority(status: &str) -> u8 {
    match status {
        "awaiting_permission" => 0,
        "resume_failed" => 1,
        "streaming" => 2,
        "idle" => 3,
        "spawning" => 4,
        "exited" => 5,
        _ => 6,
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

async fn handle_modal_key(key: KeyEvent, app: &mut App) {
    // Help modal: scroll keys scroll, esc/q/? close, anything else closes too.
    if let Some(Modal::Help { scroll }) = app.modal.as_mut() {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('q'), _) | (KeyCode::Char('?'), _) => {
                app.modal = None;
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                *scroll = scroll.saturating_sub(1);
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                *scroll = scroll.saturating_add(1);
            }
            (KeyCode::PageUp, _) => {
                *scroll = scroll.saturating_sub(8);
            }
            (KeyCode::PageDown, _) => {
                *scroll = scroll.saturating_add(8);
            }
            (KeyCode::Home, _) | (KeyCode::Char('g'), _) => {
                *scroll = 0;
            }
            _ => {
                app.modal = None;
            }
        }
        return;
    }
    // Details modal: any key closes it.
    if matches!(app.modal, Some(Modal::Details { .. })) {
        app.modal = None;
        return;
    }
    // Theme picker: nav with ↑/↓/j/k, Enter commits, Esc reverts.
    if let Some(Modal::ThemePicker { selected_idx, original_name }) = app.modal.as_mut() {
        let n = crate::theme::ALL.len();
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                if let Some(orig) = crate::theme::ALL.iter().find(|t| t.name == *original_name) {
                    crate::theme::set(orig.clone());
                }
                app.modal = None;
            }
            (KeyCode::Enter, _) => {
                let chosen = crate::theme::ALL[*selected_idx].clone();
                crate::theme::set(chosen.clone());
                crate::theme::save(&chosen);
                app.set_status(format!("theme: {}", chosen.label));
                app.modal = None;
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                if *selected_idx > 0 {
                    *selected_idx -= 1;
                    crate::theme::set(crate::theme::ALL[*selected_idx].clone());
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                if *selected_idx + 1 < n {
                    *selected_idx += 1;
                    crate::theme::set(crate::theme::ALL[*selected_idx].clone());
                }
            }
            _ => {}
        }
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
    if matches!(app.modal, Some(Modal::SpawnForm { .. })) {
        handle_spawn_form_key(key, app).await;
        return;
    }
    if matches!(app.modal, Some(Modal::WorktreeNew { .. })) {
        handle_worktree_new_key(key, app).await;
        return;
    }
}

async fn handle_spawn_form_key(key: KeyEvent, app: &mut App) {
    let mut needs_rescan = false;

    if let Some(Modal::SpawnForm {
        cwd,
        cwd_cursor,
        args,
        args_cursor,
        focus,
        recent_selected,
        repo_root,
        worktrees,
        wt_selected,
        cwd_completion,
        model,
        ..
    }) = app.modal.as_mut()
    {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                app.modal = None;
                return;
            }
            (KeyCode::Enter, _) => {
                // [+ new worktree] is a special row that opens the sub-modal
                // and never falls through to spawn.
                if *focus == FormField::Worktrees && *wt_selected == 0 {
                    if let Some(root) = repo_root.clone() {
                        let suggested_branch = crate::git::suggest_branch_name(
                            root.file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or("worktree"),
                            worktrees,
                        );
                        let suggested_path =
                            crate::git::suggest_worktree_path(&root, &suggested_branch, worktrees);
                        let path_s = suggested_path.to_string_lossy().into_owned();
                        let branch_cursor = suggested_branch.len();
                        let path_cursor = path_s.len();
                        app.modal = Some(Modal::WorktreeNew {
                            repo_root: root,
                            branch: suggested_branch,
                            branch_cursor,
                            path: path_s,
                            path_cursor,
                            focus: WtFormField::Branch,
                            error: None,
                        });
                        return;
                    }
                }

                // Enter on an existing worktree row: populate cwd from that
                // worktree, then fall through to the normal submit. Single
                // press → spawn — no second Enter required.
                if *focus == FormField::Worktrees {
                    if let Some(wt) = worktrees.get(*wt_selected - 1) {
                        *cwd = wt.path.to_string_lossy().into_owned();
                        *cwd_cursor = cwd.len();
                    }
                }

                let cwd_v = cwd.trim().to_string();
                if cwd_v.is_empty() {
                    app.set_status("cwd is empty".into());
                    return;
                }
                let cwd_path = PathBuf::from(&cwd_v);
                let mkdir_err = if !cwd_path.is_dir() {
                    std::fs::create_dir_all(&cwd_path).err().map(|e| e.to_string())
                } else {
                    None
                };
                if let Some(e) = mkdir_err {
                    app.set_status(format!("mkdir failed for {cwd_v}: {e}"));
                    return;
                }
                let extra: Vec<String> = match shell_words::split(args) {
                    Ok(v) => v,
                    Err(e) => {
                        app.set_status(format!("bad args: {e}"));
                        return;
                    }
                };
                let model_arg = model.as_arg().map(|s| s.to_string());
                app.modal = None;
                match client::create_session_raw(cwd_v.clone(), None, model_arg, extra).await {
                    Ok(_) => {
                        app.push_recent_cwd(cwd_v);
                        app.set_status("session created".into());
                        refresh_sessions(app).await;
                    }
                    Err(e) => app.set_status(format!("create failed: {e}")),
                }
                return;
            }
            (KeyCode::Tab, _) => {
                let has_repo = repo_root.is_some();
                // Visual top-to-bottom cycle: cwd → worktrees → model → flags → cwd.
                // Skip worktrees if the cwd isn't inside a git repo.
                *focus = match (*focus, has_repo) {
                    (FormField::Cwd, true) => FormField::Worktrees,
                    (FormField::Cwd, false) => FormField::Model,
                    (FormField::Worktrees, _) => FormField::Model,
                    (FormField::Model, _) => FormField::Args,
                    (FormField::Args, _) => FormField::Cwd,
                };
            }
            (KeyCode::Left, _) if *focus == FormField::Model => {
                let cur = ModelChoice::ALL.iter().position(|m| *m == *model).unwrap_or(0);
                let next = if cur == 0 { ModelChoice::ALL.len() - 1 } else { cur - 1 };
                *model = ModelChoice::ALL[next];
            }
            (KeyCode::Right, _) if *focus == FormField::Model => {
                let cur = ModelChoice::ALL.iter().position(|m| *m == *model).unwrap_or(0);
                let next = (cur + 1) % ModelChoice::ALL.len();
                *model = ModelChoice::ALL[next];
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
                        needs_rescan = true;
                    }
                }
            }
            (KeyCode::Down, _) if *focus == FormField::Cwd => {
                if *recent_selected + 1 < app.recent_cwds.len() {
                    *recent_selected += 1;
                    if let Some(pick) = app.recent_cwds.get(*recent_selected) {
                        *cwd = pick.clone();
                        *cwd_cursor = cwd.len();
                        needs_rescan = true;
                    }
                }
            }
            (KeyCode::Up, _) if *focus == FormField::Worktrees => {
                if *wt_selected > 0 {
                    *wt_selected -= 1;
                }
            }
            (KeyCode::Down, _) if *focus == FormField::Worktrees => {
                let max = worktrees.len();
                if *wt_selected < max {
                    *wt_selected += 1;
                }
            }
            // Right-arrow at end of cwd accepts the ghost-text completion.
            // Anywhere else, falls through to the normal text-input handler
            // which moves the cursor right.
            (KeyCode::Right, _)
                if *focus == FormField::Cwd && *cwd_cursor == cwd.len() =>
            {
                if let Some(suffix) = cwd_completion.clone() {
                    cwd.push_str(&suffix);
                    *cwd_cursor = cwd.len();
                    needs_rescan = true;
                }
            }
            _ => {
                let cwd_was = cwd.clone();
                let (input, cursor) = match focus {
                    FormField::Cwd => (&mut *cwd, &mut *cwd_cursor),
                    FormField::Args => (&mut *args, &mut *args_cursor),
                    FormField::Worktrees | FormField::Model => return,
                };
                handle_text_input(input, cursor, &key);
                if *focus == FormField::Cwd && *cwd != cwd_was {
                    needs_rescan = true;
                }
            }
        }
    }

    if needs_rescan {
        rescan_spawn_form(app);
    }
}

async fn handle_worktree_new_key(key: KeyEvent, app: &mut App) {
    // Snapshot inputs so we can act on them after taking the modal.
    let snapshot = if let Some(Modal::WorktreeNew {
        repo_root,
        branch,
        path,
        ..
    }) = app.modal.as_ref()
    {
        Some((repo_root.clone(), branch.clone(), path.clone()))
    } else {
        None
    };

    if let Some(Modal::WorktreeNew {
        branch,
        branch_cursor,
        path,
        path_cursor,
        focus,
        error,
        ..
    }) = app.modal.as_mut()
    {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                // Drop the new-worktree modal; spawn form remains as-is.
                app.modal = None;
                // Re-open the spawn form with the previous state? We dropped
                // it when we transitioned. Simpler: nothing to restore for now;
                // user re-presses `c`. (See note in CHANGELOG; could be improved.)
                return;
            }
            (KeyCode::Tab, _) => {
                *focus = match focus {
                    WtFormField::Branch => WtFormField::Path,
                    WtFormField::Path => WtFormField::Branch,
                };
                return;
            }
            (KeyCode::Enter, _) => {
                let (root, b, p) = match snapshot {
                    Some(t) => t,
                    None => return,
                };
                let b = b.trim().to_string();
                let p = p.trim().to_string();
                if b.is_empty() {
                    *error = Some("branch name is empty".into());
                    return;
                }
                if p.is_empty() {
                    *error = Some("path is empty".into());
                    return;
                }
                let path_buf = PathBuf::from(&p);
                if let Err(e) = crate::git::create_worktree(&root, &b, &path_buf) {
                    *error = Some(format!("{e:#}"));
                    return;
                }
                // Worktree created — re-open the spawn form with cwd set
                // to the new path so the user can immediately spawn there.
                let cursor = p.len();
                app.modal = Some(Modal::SpawnForm {
                    cwd: p.clone(),
                    cwd_cursor: cursor,
                    args: String::new(),
                    args_cursor: 0,
                    focus: FormField::Cwd,
                    recent_selected: 0,
                    repo_root: None,
                    worktrees: Vec::new(),
                    collision: false,
                    wt_selected: 0,
                    last_scan_cwd: String::new(),
                    cwd_completion: None,
                    cwd_missing: false,
                    model: ModelChoice::Default,
                });
                rescan_spawn_form(app);
                app.set_status(format!("worktree '{b}' created at {p}"));
                return;
            }
            _ => {
                *error = None;
                let (input, cursor) = match focus {
                    WtFormField::Branch => (branch, branch_cursor),
                    WtFormField::Path => (path, path_cursor),
                };
                handle_text_input(input, cursor, &key);
            }
        }
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
        (KeyCode::Char('?'), _) => app.modal = Some(Modal::Help { scroll: 0 }),
        (KeyCode::Char('g'), KeyModifiers::NONE) => {
            app.grid_mode = !app.grid_mode;
            app.set_status(if app.grid_mode { "grid view".into() } else { "sidebar view".into() });
        }
        (KeyCode::Char('t'), KeyModifiers::NONE) => {
            let cur = crate::theme::current();
            let idx = crate::theme::ALL
                .iter()
                .position(|t| t.name == cur.name)
                .unwrap_or(0);
            app.modal = Some(Modal::ThemePicker {
                selected_idx: idx,
                original_name: cur.name.to_string(),
            });
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
            // Clear caches so branch lookups + state diffs re-derive fresh.
            app.session_branches.clear();
            refresh_sessions(app).await;
            app.set_status("refreshed".into());
        }
        (KeyCode::Char('c'), KeyModifiers::NONE) => {
            // Default cwd: the most-recently-active session's cwd if any,
            // otherwise the shell's current dir. Matches the common pattern
            // of "spawn a sibling of what I was just doing."
            let default_cwd = app
                .sessions
                .iter()
                .max_by_key(|s| s.last_activity_ms)
                .map(|s| s.cwd.clone())
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|p| p.to_string_lossy().into_owned())
                })
                .unwrap_or_default();
            let cursor = default_cwd.len();
            app.modal = Some(Modal::SpawnForm {
                cwd: default_cwd,
                cwd_cursor: cursor,
                args: String::new(),
                args_cursor: 0,
                focus: FormField::Cwd,
                recent_selected: 0,
                repo_root: None,
                worktrees: Vec::new(),
                collision: false,
                wt_selected: 0,
                last_scan_cwd: String::new(),
                cwd_completion: None,
                cwd_missing: false,
                model: ModelChoice::Default,
            });
            rescan_spawn_form(app);
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
    if info.status == "resume_failed" {
        app.set_status("resume failed; press i for details, x to forget".into());
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
            repo_root,
            worktrees,
            collision,
            wt_selected,
            cwd_completion,
            cwd_missing,
            model,
            ..
        } => draw_spawn_form(
            f,
            cwd,
            *cwd_cursor,
            args,
            *args_cursor,
            *focus,
            *recent_selected,
            &app.recent_cwds,
            repo_root.as_deref(),
            worktrees,
            *collision,
            *wt_selected,
            cwd_completion.as_deref(),
            *cwd_missing,
            *model,
        ),
        Modal::WorktreeNew {
            repo_root,
            branch,
            branch_cursor,
            path,
            path_cursor,
            focus,
            error,
        } => draw_worktree_new(
            f,
            repo_root,
            branch,
            *branch_cursor,
            path,
            *path_cursor,
            *focus,
            error.as_deref(),
        ),
        Modal::Help { scroll } => draw_help(f, *scroll),
        Modal::Rename { input, cursor, session_id } => {
            let placeholder = app
                .sessions
                .iter()
                .find(|s| s.id == *session_id)
                .map(|s| {
                    s.display_override
                        .clone()
                        .or_else(|| s.ai_title.clone())
                        .unwrap_or_else(|| s.name.clone())
                })
                .unwrap_or_default();
            draw_rename(f, input, *cursor, &placeholder);
        }
        Modal::Details { session_id } => {
            if let Some(s) = app.sessions.iter().find(|s| s.id == *session_id) {
                let branch = app
                    .session_branches
                    .get(session_id)
                    .and_then(|b| b.clone());
                draw_details(f, s, branch.as_deref());
            } else {
                draw_help(f, 0);
            }
        }
        Modal::ThemePicker { selected_idx, .. } => draw_theme_picker(f, *selected_idx),
    }
}

fn draw_theme_picker(f: &mut ratatui::Frame, selected_idx: usize) {
    let theme = crate::theme::current();
    let parent = f.area();
    let n = crate::theme::ALL.len();
    let w = 50.min(parent.width.saturating_sub(4));
    let h = (n as u16) + 5;
    let area = centered_rect(w, h, parent);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(ratatui::text::Span::styled(
            " theme ",
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    use ratatui::text::{Line, Span};

    for (i, t) in crate::theme::ALL.iter().enumerate() {
        let active = i == selected_idx;
        let marker = if active { "› " } else { "  " };
        let marker_style = if active {
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.dim)
        };
        let label_style = if active {
            Style::default().fg(theme.title).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.body)
        };
        let label = format!("{:<18}", t.label);
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(marker.to_string(), marker_style),
            Span::styled(label, label_style),
            Span::styled("  ", Style::default().fg(theme.dim)),
        ];
        // Five color swatches drawn from the theme's palette.
        for c in [t.idle, t.working, t.awaiting_a, t.accent, t.cost] {
            spans.push(Span::styled(
                "● ",
                Style::default().fg(c).add_modifier(Modifier::BOLD),
            ));
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(inner.x + 1, inner.y + 1 + i as u16, inner.width.saturating_sub(2), 1),
        );
    }

    f.render_widget(
        Paragraph::new(" ↑/↓ preview  ·  enter save  ·  esc revert")
            .style(Style::default().fg(theme.dim)),
        Rect::new(
            inner.x + 1,
            inner.y + inner.height.saturating_sub(2),
            inner.width.saturating_sub(2),
            1,
        ),
    );
}

fn draw_help(f: &mut ratatui::Frame, scroll: u16) {
    let theme = crate::theme::current();
    let parent = f.area();
    let w = 78.min(parent.width.saturating_sub(4));
    let h = 42.min(parent.height.saturating_sub(2));
    let area = centered_rect(w, h, parent);
    f.render_widget(Clear, area);
    let title = format!(" claws {}  ·  keymap ", env!("CARGO_PKG_VERSION"));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(ratatui::text::Span::styled(
            title,
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    use ratatui::text::{Line, Span};
    let kbd_style = Style::default()
        .fg(theme.title)
        .bg(theme.awaiting_bg)
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(theme.body);
    let dim_style = Style::default().fg(theme.dim);

    // Section header: ▎ in theme.accent + bold label.
    let sec = |label: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                "▎ ",
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                label.to_string(),
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
        ])
    };
    // Render keys with a kbd-like padded bg pill, then description.
    let row = |k: &str, v: &str| -> Line<'static> {
        let pill = format!(" {k} ");
        let pill_w = pill.chars().count();
        // Pad after the pill out to column 24 so descriptions align.
        let pad_target = 24usize;
        let pad = pad_target.saturating_sub(pill_w + 2);
        Line::from(vec![
            Span::styled("  ", dim_style),
            Span::styled(pill, kbd_style),
            Span::styled(" ".repeat(pad), dim_style),
            Span::styled(v.to_string(), desc_style),
        ])
    };

    let mut lines: Vec<Line> = Vec::new();

    lines.push(sec("dashboard"));
    lines.push(row("h j k l / arrows", "navigate sessions"));
    lines.push(row("enter", "attach to selected session"));
    lines.push(row("c", "new session (opens spawn form)"));
    lines.push(row("g", "toggle grid / sidebar layout"));
    lines.push(row("r", "rename selected"));
    lines.push(row("R", "restart selected (kill + claude --resume)"));
    lines.push(row("x", "close & forget selected"));
    lines.push(row("/", "filter sessions"));
    lines.push(row("i", "session details popup"));
    lines.push(row("F5", "force refresh"));
    lines.push(row("t", "open theme picker"));
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
    lines.push(row("tab", "cycle cwd → worktrees → flags → cwd"));
    lines.push(row("↑ / ↓  (cwd field)", "cycle recent directories"));
    lines.push(row("↑ / ↓  (worktrees field)", "cycle worktrees"));
    lines.push(row("→  (cwd, end of line)", "accept ghost-text path completion"));
    lines.push(row("Ctrl-Y", "toggle --dangerously-skip-permissions"));
    lines.push(row("enter (cwd/flags)", "spawn the session"));
    lines.push(row("enter (worktree)", "spawn in that worktree"));
    lines.push(row("enter ([+ new])", "open the new-worktree form"));
    lines.push(row("esc", "cancel"));

    lines.push(Line::from(""));
    lines.push(sec("worktrees"));
    lines.push(row("(any git repo)", "spawn form lists worktrees automatically"));
    lines.push(row("[+ new worktree]", "branch off HEAD into a new sibling dir"));
    lines.push(row("git auth", "needed only if claude pushes/fetches inside the worktree"));

    lines.push(Line::from(""));
    lines.push(sec("from the shell  (outside the TUI)"));
    lines.push(row("claws", "open the dashboard"));
    lines.push(row("claws update", "install the latest release in place"));
    lines.push(row("claws kill-server", "stop the daemon and all sessions"));
    lines.push(row("claws logs", "print the log file path"));
    lines.push(row("claws --version", "print version"));

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑/↓ scroll  ·  esc to close",
        Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC),
    )));

    // Reserve the bottom row of the modal for a sticky scroll hint so it
    // doesn't disappear when the user scrolls down past the end of content.
    let total_lines = lines.len() as u16;
    let body_h = inner.height.saturating_sub(1);
    let max_scroll = total_lines.saturating_sub(body_h);
    let clamped = scroll.min(max_scroll);

    f.render_widget(
        Paragraph::new(lines).scroll((clamped, 0)),
        Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), body_h),
    );

    // Bottom-anchored sticky hint with scroll position.
    let hint = if max_scroll == 0 {
        " esc to close ".to_string()
    } else {
        let pct = if max_scroll == 0 { 100 } else { (clamped as u32 * 100) / max_scroll as u32 };
        format!(" ↑/↓ scroll · {pct}% · esc to close ")
    };
    let hint_w = hint.chars().count() as u16;
    let hint_x = inner.x + inner.width.saturating_sub(hint_w + 1);
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(theme.dim)),
        Rect::new(hint_x, inner.y + inner.height.saturating_sub(1), hint_w, 1),
    );
}

fn draw_rename(f: &mut ratatui::Frame, input: &str, cursor: usize, placeholder: &str) {
    let theme = crate::theme::current();
    let parent = f.area();
    let w = 60.min(parent.width.saturating_sub(4));
    let h = 5;
    let area = centered_rect(w, h, parent);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(ratatui::text::Span::styled(
            " rename ",
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let row = Rect::new(inner.x + 1, inner.y + 1, inner.width.saturating_sub(2), 1);
    if input.is_empty() && !placeholder.is_empty() {
        f.render_widget(
            Paragraph::new(placeholder.to_string())
                .style(Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC)),
            row,
        );
    } else {
        f.render_widget(
            Paragraph::new(input.to_string()).style(Style::default().fg(theme.title)),
            row,
        );
    }
    f.set_cursor_position(Position::new(row.x + cursor as u16, row.y));
    f.render_widget(
        Paragraph::new(" enter save  ·  esc cancel  ·  blank reverts to ai_title")
            .style(Style::default().fg(theme.dim)),
        Rect::new(inner.x + 1, inner.y + 2, inner.width.saturating_sub(2), 1),
    );
}

fn draw_details(f: &mut ratatui::Frame, s: &SessionInfo, branch: Option<&str>) {
    let theme = crate::theme::current();
    let parent = f.area();
    let w = 80.min(parent.width.saturating_sub(4));
    let h = 18.min(parent.height.saturating_sub(2));
    let area = centered_rect(w, h, parent);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(ratatui::text::Span::styled(
            format!(" details — {} ", s.id),
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    use ratatui::text::{Line, Span};
    let label = Style::default().fg(theme.dim);
    let val = Style::default().fg(theme.body);
    let mut lines: Vec<Line> = Vec::new();
    // Match the dashboard detail pane: 10-col label width.
    let row = |k: &'static str, v: String, vstyle: Style| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {:<10}", k), label),
            Span::styled(v, vstyle),
        ])
    };

    let title = s
        .display_override
        .clone()
        .or_else(|| s.ai_title.clone())
        .unwrap_or_else(|| s.name.clone());
    lines.push(row("title", title, Style::default().fg(theme.title).add_modifier(Modifier::BOLD)));
    lines.push(row("name", s.name.clone(), val));
    lines.push(row("status", s.status.clone(), Style::default().fg(status_color(&s.status))));
    if let Some(c) = s.exit_code {
        lines.push(row("exit", c.to_string(), val));
    }
    lines.push(row("cwd", s.cwd.clone(), Style::default().fg(theme.cwd)));
    if let Some(b) = branch {
        lines.push(row(
            "branch",
            b.to_string(),
            Style::default().fg(theme.idle).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(m) = &s.model {
        lines.push(row("model", m.clone(), Style::default().fg(theme.model)));
    }
    if let Some(t) = &s.current_tool {
        lines.push(row("tool", t.clone(), Style::default().fg(theme.tool)));
    }
    if !s.extra_args.is_empty() {
        let joined = s.extra_args.join(" ");
        let dangerous = s.extra_args.iter().any(|a| a == DANGEROUS_FLAG);
        let style = if dangerous {
            Style::default().fg(theme.context_high).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.body)
        };
        lines.push(row("flags", joined, style));
    }
    lines.push(row("turns", s.turn_count.to_string(), val));
    lines.push(row(
        "tokens",
        format!(
            "input {}  ·  output {}  ·  cache {}",
            compact_num(s.tokens_input),
            compact_num(s.tokens_output),
            compact_num(s.tokens_cache_read)
        ),
        val,
    ));
    lines.push(row("started", format_time_ago(s.started_at_ms), val));
    lines.push(row("last seen", format_time_ago(s.last_activity_ms), val));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  last message", label)));
    lines.push(Line::from(vec![
        Span::styled("  ▎ ", Style::default().fg(theme.dim)),
        Span::styled(
            s.last_message.as_deref().unwrap_or("(none)").to_string(),
            Style::default().fg(theme.body).add_modifier(Modifier::ITALIC),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  press any key to close",
        Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC),
    )));

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
    repo_root: Option<&std::path::Path>,
    worktrees: &[crate::git::Worktree],
    collision: bool,
    wt_selected: usize,
    cwd_completion: Option<&str>,
    cwd_missing: bool,
    model: ModelChoice,
) {
    use ratatui::text::{Line, Span};
    let theme = crate::theme::current();
    let parent = f.area();
    let w = 84.min(parent.width.saturating_sub(4));

    // Cap visible rows per section.
    let recents_shown: u16 = recent.len().min(3) as u16;
    let wt_count_shown: u16 = worktrees.len().min(5) as u16;
    let has_repo = repo_root.is_some();
    let has_recents = recents_shown > 0;

    // Pre-compute the row plan as a list of "row kinds" + heights so the
    // padding logic is uniform: every section is preceded by a single
    // blank row (except the first), and every section is `(label, body)`
    // with no internal gaps.
    enum RowKind {
        Pad,
        DirLabel,
        DirInput,
        RecentLabel,
        RecentList,
        CollisionBanner,
        WtLabel,
        WtList,
        ModelLabel,
        ModelPills,
        FlagsLabel,
        FlagsInput,
        SkipHint,
        Examples,
        FooterHelp,
    }

    let mut plan: Vec<(RowKind, u16)> = Vec::new();
    plan.push((RowKind::Pad, 1));
    plan.push((RowKind::DirLabel, 1));
    plan.push((RowKind::DirInput, 1));
    if has_recents {
        plan.push((RowKind::Pad, 1));
        plan.push((RowKind::RecentLabel, 1));
        plan.push((RowKind::RecentList, recents_shown));
    }
    if collision {
        plan.push((RowKind::Pad, 1));
        plan.push((RowKind::CollisionBanner, 1));
    }
    if has_repo {
        plan.push((RowKind::Pad, 1));
        plan.push((RowKind::WtLabel, 1));
        plan.push((RowKind::WtList, 1 + wt_count_shown));
    }
    plan.push((RowKind::Pad, 1));
    plan.push((RowKind::ModelLabel, 1));
    plan.push((RowKind::ModelPills, 1));
    plan.push((RowKind::Pad, 1));
    plan.push((RowKind::FlagsLabel, 1));
    plan.push((RowKind::FlagsInput, 1));
    plan.push((RowKind::SkipHint, 1));
    plan.push((RowKind::Examples, 1));
    plan.push((RowKind::Pad, 1));
    plan.push((RowKind::FooterHelp, 1));
    plan.push((RowKind::Pad, 1));

    let inner_h: u16 = plan.iter().map(|(_, h)| *h).sum();
    let h = inner_h + 2; // borders
    let area = centered_rect(w, h, parent);

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(ratatui::text::Span::styled(
            " spawn session ",
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // 3-col left padding (1 from inner + 2 of breathing).
    let content_x = inner.x + 3;
    let content_w = inner.width.saturating_sub(6); // 3 each side

    let constraints: Vec<Constraint> =
        plan.iter().map(|(_, h)| Constraint::Length(*h)).collect();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(Rect::new(
            content_x,
            inner.y,
            content_w,
            inner.height,
        ));

    let label_dim = Style::default().fg(theme.dim);
    let label_active = Style::default().fg(theme.accent).add_modifier(Modifier::BOLD);

    // Track input row indices for cursor placement.
    let mut idx_dir_input: Option<usize> = None;
    let mut idx_flags_input: Option<usize> = None;

    for (i, (kind, _)) in plan.iter().enumerate() {
        let area = chunks[i];
        match kind {
            RowKind::Pad => {}
            RowKind::DirLabel => {
                f.render_widget(
                    Paragraph::new("directory").style(if focus == FormField::Cwd {
                        label_active
                    } else {
                        label_dim
                    }),
                    area,
                );
            }
            RowKind::DirInput => {
                idx_dir_input = Some(i);
                let mut spans: Vec<Span<'static>> = vec![Span::styled(
                    cwd.to_string(),
                    Style::default().fg(theme.title),
                )];
                if focus == FormField::Cwd {
                    if let Some(suffix) = cwd_completion {
                        if !suffix.is_empty() {
                            spans.push(Span::styled(
                                suffix.to_string(),
                                Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC),
                            ));
                        }
                    }
                }
                // Missing-directory hint: only when no completion is offered
                // (the two would visually conflict). The hint colour matches
                // the "needs you" warn palette so it reads as actionable.
                if cwd_missing && cwd_completion.map(|s| s.is_empty()).unwrap_or(true) {
                    spans.push(Span::styled(
                        "  [enter to mkdir -p]".to_string(),
                        Style::default()
                            .fg(theme.context_high)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
                f.render_widget(Paragraph::new(Line::from(spans)), area);
            }
            RowKind::RecentLabel => {
                f.render_widget(
                    Paragraph::new("recent").style(label_dim),
                    area,
                );
            }
            RowKind::RecentList => {
                let inner_w = area.width as usize;
                for (j, dir) in recent.iter().take(recents_shown as usize).enumerate() {
                    let y = area.y + j as u16;
                    let active = j == recent_selected && focus == FormField::Cwd;
                    let marker = if active { "› " } else { "  " };
                    let marker_style = if active {
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.dim)
                    };
                    let basename = cwd_basename(dir);
                    let display_path = shorten_home(dir);
                    let bn = format!("{:<16}", truncate_ellipsis(&basename, 16));
                    let bn_w = bn.chars().count();
                    let path_avail = inner_w.saturating_sub(bn_w + 2 + 2);
                    let path_short = truncate_ellipsis(&display_path, path_avail);
                    let name_style = if active {
                        Style::default().fg(theme.title).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.body)
                    };
                    let path_style = Style::default().fg(theme.dim);
                    let line = Line::from(vec![
                        Span::styled(marker.to_string(), marker_style),
                        Span::styled(bn, name_style),
                        Span::styled("  ", path_style),
                        Span::styled(path_short, path_style),
                    ]);
                    f.render_widget(
                        Paragraph::new(line),
                        Rect::new(area.x, y, area.width, 1),
                    );
                }
            }
            RowKind::CollisionBanner => {
                f.render_widget(
                    Paragraph::new("⚠  session already running in this directory")
                        .style(Style::default().fg(theme.context_high).add_modifier(Modifier::BOLD)),
                    area,
                );
            }
            RowKind::WtLabel => {
                let repo_label = repo_root
                    .and_then(|r| r.file_name())
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                let style = if focus == FormField::Worktrees { label_active } else { label_dim };
                f.render_widget(
                    Paragraph::new(format!("worktrees · {repo_label}")).style(style),
                    area,
                );
            }
            RowKind::WtList => {
                // Row 0: [+ new worktree]
                let new_active = wt_selected == 0 && focus == FormField::Worktrees;
                let new_marker = if new_active { "› " } else { "  " };
                let marker_style = if new_active {
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.dim)
                };
                let new_label_style = if new_active {
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.accent)
                };
                let new_line = Line::from(vec![
                    Span::styled(new_marker.to_string(), marker_style),
                    Span::styled("[+ new worktree]", new_label_style),
                ]);
                f.render_widget(
                    Paragraph::new(new_line),
                    Rect::new(area.x, area.y, area.width, 1),
                );

                let inner_w = area.width as usize;
                for (j, wt) in worktrees.iter().take(wt_count_shown as usize).enumerate() {
                    let y = area.y + 1 + j as u16;
                    let active = focus == FormField::Worktrees && wt_selected == j + 1;
                    let is_current = paths_equal(&wt.path.to_string_lossy(), cwd);
                    let marker = if active { "› " } else { "  " };
                    let marker_style = if active {
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.dim)
                    };
                    let branch = wt.branch.clone().unwrap_or_else(|| "(detached)".into());
                    let branch_padded = format!("{:<16}", truncate_ellipsis(&branch, 16));
                    let path_str = wt.path.to_string_lossy().into_owned();
                    let path_disp = shorten_home(&path_str);
                    let bn_w = branch_padded.chars().count();
                    let suffix = if is_current { "  (current)" } else { "" };
                    let suffix_w = suffix.chars().count();
                    let path_avail = inner_w.saturating_sub(bn_w + 2 + suffix_w + 2);
                    let path_short = truncate_ellipsis(&path_disp, path_avail);
                    let branch_style = if active {
                        Style::default().fg(theme.title).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.model)
                    };
                    let path_style = Style::default().fg(theme.dim);
                    let suffix_style = Style::default().fg(theme.idle).add_modifier(Modifier::ITALIC);
                    let line = Line::from(vec![
                        Span::styled(marker.to_string(), marker_style),
                        Span::styled(branch_padded, branch_style),
                        Span::styled("  ", path_style),
                        Span::styled(path_short, path_style),
                        Span::styled(suffix.to_string(), suffix_style),
                    ]);
                    f.render_widget(
                        Paragraph::new(line),
                        Rect::new(area.x, y, area.width, 1),
                    );
                }
            }
            RowKind::ModelLabel => {
                f.render_widget(
                    Paragraph::new("model").style(if focus == FormField::Model {
                        label_active
                    } else {
                        label_dim
                    }),
                    area,
                );
            }
            RowKind::ModelPills => {
                let active_field = focus == FormField::Model;
                let mut spans: Vec<Span<'static>> = Vec::new();
                for (idx, m) in ModelChoice::ALL.iter().enumerate() {
                    let selected = *m == model;
                    let pill = format!(" {} ", m.label());
                    let style = match (selected, active_field) {
                        (true, true) => Style::default()
                            .fg(theme.body)
                            .bg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                        (true, false) => Style::default()
                            .fg(theme.title)
                            .add_modifier(Modifier::BOLD),
                        (false, _) => Style::default().fg(theme.dim),
                    };
                    spans.push(Span::styled(pill, style));
                    if idx + 1 < ModelChoice::ALL.len() {
                        spans.push(Span::styled("  ", Style::default().fg(theme.dim)));
                    }
                }
                if active_field {
                    spans.push(Span::styled("    ", Style::default().fg(theme.dim)));
                    spans.push(Span::styled(
                        "← →",
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::styled(
                        " cycle",
                        Style::default().fg(theme.dim),
                    ));
                }
                f.render_widget(Paragraph::new(Line::from(spans)), area);
            }
            RowKind::FlagsLabel => {
                f.render_widget(
                    Paragraph::new("flags").style(if focus == FormField::Args {
                        label_active
                    } else {
                        label_dim
                    }),
                    area,
                );
            }
            RowKind::FlagsInput => {
                idx_flags_input = Some(i);
                let display = if args.is_empty() {
                    if focus == FormField::Args {
                        String::new()
                    } else {
                        "(none)".to_string()
                    }
                } else {
                    args.to_string()
                };
                let style = if args.is_empty() && focus != FormField::Args {
                    Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC)
                } else {
                    Style::default().fg(theme.title)
                };
                f.render_widget(Paragraph::new(display).style(style), area);
            }
            RowKind::SkipHint => {
                let skip_flag = "--dangerously-skip-permissions";
                let skip_on = args.split_whitespace().any(|t| t == skip_flag);
                let state_style = if skip_on {
                    Style::default().fg(theme.context_high).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.dim)
                };
                let line = Line::from(vec![
                    Span::styled(
                        "ctrl-y ",
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("skip permissions: ", Style::default().fg(theme.dim)),
                    Span::styled(if skip_on { "on" } else { "off" }, state_style),
                ]);
                f.render_widget(Paragraph::new(line), area);
            }
            RowKind::Examples => {
                f.render_widget(
                    Paragraph::new("examples: --effort xhigh · -p \"…\" · --add-dir <path>")
                        .style(Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC)),
                    area,
                );
            }
            RowKind::FooterHelp => {
                let key = Style::default().fg(theme.accent).add_modifier(Modifier::BOLD);
                let desc = Style::default().fg(theme.dim);
                let pair = |k: &'static str, v: &'static str| -> [Span<'static>; 3] {
                    [
                        Span::styled(k.to_string(), key),
                        Span::raw(" "),
                        Span::styled(v.to_string(), desc),
                    ]
                };
                let gap = || Span::styled("    ", desc);
                let mut spans: Vec<Span<'static>> = Vec::new();
                spans.extend(pair("enter", "create"));
                spans.push(gap());
                spans.extend(pair("tab", "next field"));
                spans.push(gap());
                spans.extend(pair("↑↓", "navigate"));
                spans.push(gap());
                spans.extend(pair("esc", "cancel"));
                f.render_widget(Paragraph::new(Line::from(spans)), area);
            }
        }
    }

    // Cursor placement (after rendering).
    match (focus, idx_dir_input, idx_flags_input) {
        (FormField::Cwd, Some(i), _) => {
            let row = chunks[i];
            f.set_cursor_position(Position::new(row.x + cwd_cursor as u16, row.y));
        }
        (FormField::Args, _, Some(i)) => {
            let row = chunks[i];
            f.set_cursor_position(Position::new(row.x + args_cursor as u16, row.y));
        }
        _ => {}
    }
}

fn draw_worktree_new(
    f: &mut ratatui::Frame,
    repo_root: &std::path::Path,
    branch: &str,
    branch_cursor: usize,
    path: &str,
    path_cursor: usize,
    focus: WtFormField,
    error: Option<&str>,
) {
    let theme = crate::theme::current();
    let parent = f.area();
    let w = 80.min(parent.width.saturating_sub(4));
    let h: u16 = if error.is_some() { 12 } else { 11 };
    let area = centered_rect(w, h, parent);

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(ratatui::text::Span::styled(
            " new worktree ",
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    use ratatui::text::{Line, Span};
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1), // 0 repo label
        Constraint::Length(1), // 1 branch label
        Constraint::Length(1), // 2 branch input
        Constraint::Length(1), // 3 path label
        Constraint::Length(1), // 4 path input
        Constraint::Length(1), // 5 gap
        Constraint::Length(1), // 6 git auth tip
        Constraint::Length(1), // 7 help footer
    ];
    let mut idx_error: Option<usize> = None;
    if error.is_some() {
        idx_error = Some(constraints.len());
        constraints.push(Constraint::Length(1));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(Rect::new(
            inner.x + 1,
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        ));

    let label_dim = Style::default().fg(theme.dim);
    let label_active = Style::default().fg(theme.accent).add_modifier(Modifier::BOLD);

    let repo_short = shorten_home(&repo_root.to_string_lossy());
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("repo ", label_dim),
            Span::styled(repo_short, Style::default().fg(theme.cwd)),
        ])),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new("branch")
            .style(if focus == WtFormField::Branch { label_active } else { label_dim }),
        chunks[1],
    );
    f.render_widget(
        Paragraph::new(branch.to_string()).style(Style::default().fg(theme.title)),
        chunks[2],
    );
    f.render_widget(
        Paragraph::new("worktree path")
            .style(if focus == WtFormField::Path { label_active } else { label_dim }),
        chunks[3],
    );
    f.render_widget(
        Paragraph::new(path.to_string()).style(Style::default().fg(theme.title)),
        chunks[4],
    );

    match focus {
        WtFormField::Branch => {
            f.set_cursor_position(Position::new(
                chunks[2].x + branch_cursor as u16,
                chunks[2].y,
            ));
        }
        WtFormField::Path => {
            f.set_cursor_position(Position::new(
                chunks[4].x + path_cursor as u16,
                chunks[4].y,
            ));
        }
    }

    f.render_widget(
        Paragraph::new(
            "tip: git auth (gh, ssh keys, etc.) needs to be set up if claude will push or fetch from this worktree",
        )
        .style(Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC)),
        chunks[6],
    );

    f.render_widget(
        Paragraph::new(" enter create   ·   tab switch field   ·   esc back to spawn form")
            .style(Style::default().fg(theme.dim)),
        chunks[7],
    );

    if let (Some(i), Some(msg)) = (idx_error, error) {
        f.render_widget(
            Paragraph::new(format!(" ✗ {msg}"))
                .style(Style::default().fg(theme.context_high).add_modifier(Modifier::BOLD)),
            chunks[i],
        );
    }
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

    // Title row: theme-accent "claws" + dim count on the left,
    // right-aligned state counts (e.g. "3●  1◐  1★") in theme status colors.
    let count_part = format!(
        "{} session{}",
        app.sessions.len(),
        if app.sessions.len() == 1 { "" } else { "s" }
    );
    let title_w = chunks[0].width as usize;
    use ratatui::text::{Line, Span};

    // Aggregate per-status counts.
    let mut idle_n = 0u32;
    let mut working_n = 0u32;
    let mut awaiting_n = 0u32;
    let mut spawning_n = 0u32;
    let mut exited_n = 0u32;
    let mut failed_n = 0u32;
    for s in &app.sessions {
        match s.status.as_str() {
            "idle" => idle_n += 1,
            "streaming" => working_n += 1,
            "awaiting_permission" => awaiting_n += 1,
            "spawning" => spawning_n += 1,
            "exited" => exited_n += 1,
            "resume_failed" => failed_n += 1,
            _ => {}
        }
    }

    let mut left_spans = vec![
        Span::styled(
            " claws ",
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(theme.dim)),
        Span::styled(count_part.clone(), Style::default().fg(theme.body)),
    ];

    // Build right-aligned state-count spans. Each non-zero status emits
    // "<count><glyph>" in its theme color, separated by 2 spaces.
    let mut right_spans: Vec<Span<'static>> = Vec::new();
    let mut right_text_w: usize = 0;
    let push_count = |right_spans: &mut Vec<Span<'static>>,
                      right_text_w: &mut usize,
                      n: u32,
                      glyph: &'static str,
                      color: Color| {
        if n == 0 {
            return;
        }
        if !right_spans.is_empty() {
            right_spans.push(Span::styled("  ", Style::default().fg(theme.dim)));
            *right_text_w += 2;
        }
        let txt = format!("{n}{glyph}");
        *right_text_w += txt.chars().count();
        right_spans.push(Span::styled(
            txt,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    };
    push_count(&mut right_spans, &mut right_text_w, failed_n, "✗", theme.context_high);
    push_count(&mut right_spans, &mut right_text_w, awaiting_n, "★", theme.awaiting_a);
    push_count(&mut right_spans, &mut right_text_w, working_n, "◐", theme.working);
    push_count(&mut right_spans, &mut right_text_w, idle_n, "●", theme.idle);
    push_count(&mut right_spans, &mut right_text_w, spawning_n, "◌", theme.spawning);
    push_count(&mut right_spans, &mut right_text_w, exited_n, "○", theme.exited);

    let left_w: usize = left_spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = title_w
        .saturating_sub(left_w + right_text_w)
        .saturating_sub(1);
    if !right_spans.is_empty() {
        left_spans.push(Span::raw(" ".repeat(pad)));
        left_spans.extend(right_spans);
        left_spans.push(Span::raw(" "));
    }
    f.render_widget(
        Paragraph::new(Line::from(left_spans)),
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
                    .style(Style::default().fg(theme.dim)),
                chunks[1],
            );
        } else {
            draw_empty_state(f, chunks[1]);
        }
    } else {
        let owned: Vec<SessionInfo> = visible.iter().map(|s| (*s).clone()).collect();
        // Branch on layout mode. Both modes write back grid_cols so the
        // navigation arithmetic (move_left/right/up/down) lines up.
        let cols_used = if app.grid_mode {
            draw_grid(f, &owned, app.selected, app.tick_phase, chunks[1], &app.last_status)
        } else {
            draw_split(f, &owned, app.selected, app.tick_phase, chunks[1], app.detail_scroll, &app.last_status, &app.session_branches);
            1
        };
        let app_ptr = app as *const App as *mut App;
        unsafe { (*app_ptr).grid_cols = cols_used; }
    }

    // Footer separator + help/status line
    let footer_w = chunks[2].width as usize;
    f.render_widget(
        Paragraph::new("─".repeat(footer_w)).style(Style::default().fg(theme.dim)),
        Rect::new(chunks[2].x, chunks[2].y, chunks[2].width, 1),
    );
    if let Some(filter) = app.filter.as_deref() {
        let txt = format!(" / {filter}_  (esc clear · enter exit input)");
        f.render_widget(
            Paragraph::new(txt).style(Style::default().fg(theme.dim)),
            Rect::new(chunks[2].x, chunks[2].y + 1, chunks[2].width, 1),
        );
    } else if let Some((msg, _)) = &app.status_message {
        f.render_widget(
            Paragraph::new(format!(" {msg}")).style(Style::default().fg(theme.dim)),
            Rect::new(chunks[2].x, chunks[2].y + 1, chunks[2].width, 1),
        );
    } else {
        // Trimmed key/desc strip with the keys lit up in theme.accent.
        let key_style = Style::default().fg(theme.accent).add_modifier(Modifier::BOLD);
        let desc_style = Style::default().fg(theme.dim);
        let pair = |k: &'static str, v: &'static str| -> [Span<'static>; 3] {
            [
                Span::styled(k.to_string(), key_style),
                Span::raw(" "),
                Span::styled(v.to_string(), desc_style),
            ]
        };
        let gap = || Span::styled("    ", desc_style);
        // Show what `g` toggles to next, and which theme is active right now —
        // both bits of state the user can otherwise only learn by trying.
        let layout_label = if app.grid_mode { "sidebar" } else { "grid" };
        let theme_label = theme.label;
        let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
        spans.extend(pair("enter", "attach"));
        spans.push(gap());
        spans.extend(pair("c", "new"));
        spans.push(gap());
        spans.extend(pair("g", layout_label));
        spans.push(gap());
        spans.extend(pair("/", "filter"));
        spans.push(gap());
        spans.extend(pair("t", theme_label));
        spans.push(gap());
        spans.extend(pair("?", "help"));
        spans.push(gap());
        spans.extend(pair("q", "quit"));
        f.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(chunks[2].x, chunks[2].y + 1, chunks[2].width, 1),
        );
    }
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
    last_status: &std::collections::HashMap<Uuid, (String, SystemTime)>,
    branch_lookup: &std::collections::HashMap<Uuid, Option<String>>,
) {
    let theme = crate::theme::current();
    if area.width < SIDEBAR_W + 30 {
        // Terminal too narrow for split — fall back to a stacked single column.
        draw_sidebar(f, sessions, selected, tick_phase, area, last_status);
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

    draw_sidebar(f, sessions, selected, tick_phase, chunks[0], last_status);
    // Vertical separator in theme color
    for y in chunks[1].y..chunks[1].y + chunks[1].height {
        f.render_widget(
            Paragraph::new("│").style(Style::default().fg(theme.dim)),
            Rect::new(chunks[1].x, y, 1, 1),
        );
    }
    if let Some(s) = sessions.get(selected) {
        draw_detail(f, s, tick_phase, chunks[2], detail_scroll, branch_lookup);
    }
}

fn draw_sidebar(
    f: &mut ratatui::Frame,
    sessions: &[SessionInfo],
    selected: usize,
    tick_phase: u32,
    area: Rect,
    last_status: &std::collections::HashMap<Uuid, (String, SystemTime)>,
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
        let flash_ms = last_status
            .get(&s.id)
            .and_then(|(_, t)| t.elapsed().ok())
            .map(|d| d.as_millis())
            .filter(|&ms| ms <= 500);
        draw_sidebar_entry(
            f,
            s,
            idx == selected,
            tick_phase,
            Rect::new(area.x, y, area.width, 2),
            flash_ms,
        );
    }
}

fn draw_sidebar_entry(
    f: &mut ratatui::Frame,
    s: &SessionInfo,
    selected: bool,
    tick_phase: u32,
    area: Rect,
    flash_ms: Option<u128>,
) {
    let theme = crate::theme::current();
    let color = status_color_pulsed(&s.status, tick_phase);
    let glyph = status_glyph(&s.status, tick_phase);
    let _awaiting = s.status == "awaiting_permission";
    // The bg tint signals "something just happened" (state transition),
    // not "this row needs you" — that's already conveyed by the pulsing
    // glyph + colored label. Persistent tint competed with the selection
    // gutter and made awaiting rows look like the selected one.
    let flashing = flash_ms.is_some();
    let tint = flashing;

    // Awaiting-permission rows get a row-wide subtle bg tint so they're
    // visible from across the room. Painted before any other row content
    // so subsequent renders stack on top with their own styles intact.
    if tint {
        for r in 0..2u16 {
            f.render_widget(
                Paragraph::new(" ".repeat(area.width as usize))
                    .style(Style::default().bg(theme.awaiting_bg)),
                Rect::new(area.x, area.y + r, area.width, 1),
            );
        }
    }

    // Selected entry: 1-col theme-accent gutter on the left across both rows.
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
    let mut title_style = if title_is_auto {
        Style::default().fg(theme.title).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.title_fallback).add_modifier(Modifier::ITALIC)
    };
    if selected {
        title_style = title_style.add_modifier(Modifier::BOLD);
    }
    if tint {
        title_style = title_style.bg(theme.awaiting_bg);
    }

    use ratatui::text::{Line, Span};

    let dangerous = s.extra_args.iter().any(|a| a == DANGEROUS_FLAG);

    // Row 1: glyph + title (+ "!" suffix if spawned with --dangerously-skip-permissions)
    let danger_w = if dangerous { 2usize } else { 0 };
    let title_avail = area.width.saturating_sub(4 + danger_w as u16) as usize;
    let title_truncated = truncate_ellipsis(&title, title_avail);
    let mut glyph_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    if tint {
        glyph_style = glyph_style.bg(theme.awaiting_bg);
    }
    let row_pad_style = if tint {
        Style::default().bg(theme.awaiting_bg)
    } else {
        Style::default()
    };
    let mut danger_style = Style::default().fg(theme.context_high).add_modifier(Modifier::BOLD);
    if tint {
        danger_style = danger_style.bg(theme.awaiting_bg);
    }
    let mut title_spans = vec![
        Span::styled("  ", row_pad_style),
        Span::styled(format!("{glyph} "), glyph_style),
        Span::styled(title_truncated, title_style),
    ];
    if dangerous {
        title_spans.push(Span::styled(" !", danger_style));
    }
    let title_line = Line::from(title_spans);
    f.render_widget(
        Paragraph::new(title_line),
        Rect::new(area.x + 1, area.y, area.width.saturating_sub(1), 1),
    );

    // Row 2: indented status · time-ago, with cwd basename right-aligned.
    let status_label = display_status(&s.status);
    let time_ago = format_time_ago(s.last_activity_ms);
    let exit_part = s.exit_code.map(|c| format!(" · exit {c}")).unwrap_or_default();
    let basename = cwd_basename(&s.cwd);
    let left_text = format!("    {status_label} · {time_ago}{exit_part}");
    let inner_w = area.width.saturating_sub(1) as usize;
    let right_w = basename.chars().count();
    let pad = inner_w
        .saturating_sub(left_text.chars().count())
        .saturating_sub(right_w)
        .saturating_sub(1);
    let pad_str = " ".repeat(pad);

    let mut status_style = Style::default().fg(color);
    let mut dim_style = Style::default().fg(theme.dim);
    let mut cwd_style = Style::default().fg(theme.cwd);
    if tint {
        status_style = status_style.bg(theme.awaiting_bg);
        dim_style = dim_style.bg(theme.awaiting_bg);
        cwd_style = cwd_style.bg(theme.awaiting_bg);
    }

    let meta_line = Line::from(vec![
        Span::styled("    ", row_pad_style),
        Span::styled(status_label.to_string(), status_style),
        Span::styled(format!(" · {time_ago}"), dim_style),
        Span::styled(exit_part, dim_style),
        Span::styled(pad_str, row_pad_style),
        Span::styled(basename, cwd_style),
        Span::styled(" ", row_pad_style),
    ]);
    f.render_widget(
        Paragraph::new(meta_line),
        Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(1), 1),
    );
}

fn cwd_basename(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

/// Compare two cwd strings as filesystem paths. Tries canonicalize first
/// (handles symlinks + case-insensitive Windows drives) and falls back to
/// raw Path equality if canonicalize fails.
fn paths_equal(a: &str, b: &str) -> bool {
    let pa = std::path::Path::new(a);
    let pb = std::path::Path::new(b);
    let ca = std::fs::canonicalize(pa).ok();
    let cb = std::fs::canonicalize(pb).ok();
    match (ca, cb) {
        (Some(x), Some(y)) => x == y,
        _ => pa == pb,
    }
}

/// Recompute the worktree-aware fields on the spawn-form modal whenever
/// the user edits the cwd field. Cheap (~20ms total: two git invocations
/// plus a read_dir for completion) and only runs on actual change, gated
/// by `last_scan_cwd`.
fn rescan_spawn_form(app: &mut App) {
    let sessions = app.sessions.clone();
    if let Some(Modal::SpawnForm {
        cwd,
        repo_root,
        worktrees,
        collision,
        last_scan_cwd,
        wt_selected,
        cwd_completion,
        cwd_missing,
        ..
    }) = app.modal.as_mut()
    {
        if *cwd == *last_scan_cwd {
            return;
        }
        *last_scan_cwd = cwd.clone();
        let cwd_path = std::path::Path::new(cwd);
        *repo_root = if cwd_path.is_dir() {
            crate::git::find_repo_root(cwd_path)
        } else {
            None
        };
        *worktrees = match repo_root.as_deref() {
            Some(r) => crate::git::list_worktrees(r),
            None => Vec::new(),
        };
        *collision = sessions.iter().any(|s| paths_equal(&s.cwd, cwd));
        *wt_selected = 0;
        *cwd_completion = complete_path(cwd);
        *cwd_missing = !cwd.trim().is_empty() && !cwd_path.is_dir();
    }
}

/// Filesystem path completion: given a typed-so-far path, return the
/// suffix that would make it match a real subdirectory in the parent.
/// Picks the lexicographically first matching directory when there are
/// several. Returns None if the typed path is already complete (the
/// final component is an exact match) or no candidate exists.
fn complete_path(typed: &str) -> Option<String> {
    if typed.is_empty() {
        return None;
    }
    let path = std::path::Path::new(typed);
    let ends_with_sep = typed.ends_with('/') || typed.ends_with('\\');
    let (dir, prefix) = if ends_with_sep {
        (path.to_path_buf(), String::new())
    } else {
        let parent = path.parent()?.to_path_buf();
        let name = path.file_name().and_then(|s| s.to_str())?;
        (parent, name.to_string())
    };
    if !dir.is_dir() {
        return None;
    }
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut matches: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_str()?.to_string();
            // Prefix match (case-sensitive on Unix; case-insensitive on
            // Windows where filesystem is itself case-insensitive).
            #[cfg(unix)]
            let hit = n.starts_with(&prefix);
            #[cfg(windows)]
            let hit = n.to_lowercase().starts_with(&prefix.to_lowercase());
            if hit && e.path().is_dir() {
                Some(n)
            } else {
                None
            }
        })
        .collect();
    matches.sort();
    let first = matches.into_iter().next()?;
    // Compare in a way that matches our case sensitivity above.
    #[cfg(unix)]
    let exact = first == prefix;
    #[cfg(windows)]
    let exact = first.to_lowercase() == prefix.to_lowercase();
    if exact {
        return None;
    }
    Some(first[prefix.len()..].to_string())
}

fn draw_detail(
    f: &mut ratatui::Frame,
    s: &SessionInfo,
    tick_phase: u32,
    area: Rect,
    detail_scroll: u16,
    branch_lookup: &std::collections::HashMap<Uuid, Option<String>>,
) {
    let theme = crate::theme::current();
    let color = status_color_pulsed(&s.status, tick_phase);
    let glyph = status_glyph(&s.status, tick_phase);
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

    let pad = Rect::new(
        area.x + 2,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );

    // Header: glyph  title  ·  status  ·  uptime  ·  time-ago    (model lives in info section now)
    let status_label = display_status(&s.status);
    let uptime = format_uptime(s.started_at_ms);
    let time_ago = format_time_ago(s.last_activity_ms);
    let header_line = Line::from(vec![
        Span::styled(format!("{glyph}  "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(title, title_style),
        Span::styled("  ·  ", Style::default().fg(theme.dim)),
        Span::styled(status_label, Style::default().fg(color)),
        Span::styled(format!("  ·  up {uptime}"), Style::default().fg(theme.dim)),
        Span::styled(format!("  ·  {time_ago}"), Style::default().fg(theme.dim)),
    ]);
    f.render_widget(Paragraph::new(header_line), Rect::new(pad.x, pad.y, pad.width, 1));

    // Separator
    f.render_widget(
        Paragraph::new("─".repeat(pad.width as usize))
            .style(Style::default().fg(theme.dim)),
        Rect::new(pad.x, pad.y + 1, pad.width, 1),
    );

    let cost = s.model.as_deref().map(|m| estimate_cost(m, s)).unwrap_or(0.0);
    let cwd = shorten_path_left(&shorten_home(&s.cwd), pad.width.saturating_sub(12) as usize);
    let model = s.model.as_deref().map(short_model).unwrap_or("—").to_string();
    let label = Style::default().fg(theme.dim);
    let val = Style::default().fg(theme.body);
    let row = |k: &'static str, v: String, vstyle: Style| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{:<10}", k), label),
            Span::styled(v, vstyle),
        ])
    };

    let mut info_lines: Vec<Line> = Vec::new();
    info_lines.push(row("cwd", cwd, Style::default().fg(theme.cwd)));
    if let Some(Some(branch)) = branch_lookup.get(&s.id) {
        info_lines.push(row(
            "branch",
            branch.clone(),
            Style::default().fg(theme.idle).add_modifier(Modifier::BOLD),
        ));
    }
    info_lines.push(row("model", model, Style::default().fg(theme.model)));
    info_lines.push(row("turns", s.turn_count.to_string(), val));
    if let Some(t) = &s.current_tool {
        info_lines.push(row(
            "tool",
            format!("⚙ {}", truncate_ellipsis(t, pad.width.saturating_sub(14) as usize)),
            Style::default().fg(theme.tool),
        ));
    }
    if !s.extra_args.is_empty() {
        let joined = s.extra_args.join(" ");
        let dangerous = s.extra_args.iter().any(|a| a == DANGEROUS_FLAG);
        let truncated = truncate_ellipsis(&joined, pad.width.saturating_sub(12) as usize);
        let style = if dangerous {
            Style::default().fg(theme.context_high).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.body)
        };
        info_lines.push(row("flags", truncated, style));
    }
    let info_h = info_lines.len() as u16;
    f.render_widget(
        Paragraph::new(info_lines),
        Rect::new(pad.x, pad.y + 3, pad.width, info_h),
    );

    // Last message section. Reserve bottom 2 rows for stats strip (1 row strip + 1 gap).
    let msg_y = pad.y + 3 + info_h + 1;
    let bottom = pad.y + pad.height;
    let strip_h: u16 = 1;
    let strip_top = bottom.saturating_sub(strip_h);
    if msg_y < strip_top {
        f.render_widget(
            Paragraph::new("last message").style(label),
            Rect::new(pad.x, msg_y, pad.width, 1),
        );
        let msg_text = match s.last_message.as_deref() {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => "(no messages yet)".to_string(),
        };
        let msg_style = if s.last_message.is_some() {
            Style::default().fg(theme.body).add_modifier(Modifier::ITALIC)
        } else {
            Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC)
        };
        let msg_top = msg_y + 1;
        // Keep one blank row between the message and the stats strip.
        let msg_h = strip_top.saturating_sub(msg_top + 1).max(1);
        if msg_top < strip_top {
            // Quoted-block treatment: a left bar in theme.dim, message indented past it.
            // Bar runs only as many rows as the wrapped text actually fills,
            // not the whole message slot — otherwise short messages get a
            // tall floating column of ▎ next to nothing.
            let content_w = pad.width.saturating_sub(1).max(1) as usize;
            let mut text_rows = 0u16;
            for line in msg_text.split('\n') {
                let len = line.chars().count();
                let rows = ((len + content_w - 1) / content_w).max(1) as u16;
                text_rows = text_rows.saturating_add(rows);
                if text_rows >= msg_h {
                    break;
                }
            }
            let bar_rows = text_rows.min(msg_h).max(1);
            for r in 0..bar_rows {
                f.render_widget(
                    Paragraph::new("▎").style(Style::default().fg(theme.dim)),
                    Rect::new(pad.x, msg_top + r, 1, 1),
                );
            }
            f.render_widget(
                Paragraph::new(format!(" {msg_text}"))
                    .style(msg_style)
                    .wrap(Wrap { trim: false })
                    .scroll((detail_scroll, 0)),
                Rect::new(pad.x + 1, msg_top, pad.width.saturating_sub(1), msg_h),
            );
        }
    }

    // Bottom stats strip: tokens │ context │ cost
    if pad.height >= 2 {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let sep = || Span::styled("  │  ", Style::default().fg(theme.dim));
        let bold = Modifier::BOLD;

        // Block 1: tokens
        spans.push(Span::styled(
            format!("{}", compact_num(s.tokens_input)),
            Style::default().fg(theme.body).add_modifier(bold),
        ));
        spans.push(Span::styled(" → ", Style::default().fg(theme.dim)));
        spans.push(Span::styled(
            format!("{}", compact_num(s.tokens_output)),
            Style::default().fg(theme.body).add_modifier(bold),
        ));
        if s.tokens_cache_read > 0 {
            spans.push(Span::styled(
                format!("  cache {}", compact_num(s.tokens_cache_read)),
                Style::default().fg(theme.dim),
            ));
        }

        // Block 2: context
        spans.push(sep());
        if let Some(pct) = s.context_pct {
            let bar = context_bar(pct, 12);
            let used = s.context_used.as_deref().unwrap_or("?");
            let total = s.context_total.as_deref().unwrap_or("?");
            let bar_color = match pct {
                0..=59 => theme.context_low,
                60..=84 => theme.context_mid,
                _ => theme.context_high,
            };
            spans.push(Span::styled(bar, Style::default().fg(bar_color)));
            spans.push(Span::styled(
                format!("  {pct}%"),
                Style::default().fg(theme.body).add_modifier(bold),
            ));
            spans.push(Span::styled(
                format!("  ·  {used}/{total}"),
                Style::default().fg(theme.dim),
            ));
        } else {
            spans.push(Span::styled(
                "ctx —",
                Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC),
            ));
        }

        // Block 3: cost
        spans.push(sep());
        spans.push(Span::styled(
            format_cost(cost),
            Style::default().fg(theme.cost).add_modifier(bold),
        ));

        f.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(pad.x, strip_top, pad.width, 1),
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
            // Fast 2-tick pulse so "needs you" sessions are unmissable.
            if (tick_phase / 2) % 2 == 0 {
                t.awaiting_a
            } else {
                t.awaiting_b
            }
        }
        "exited" => t.exited,
        // Resume failure surfaces in the same palette as the high-context
        // warning — these need attention.
        "resume_failed" => t.context_high,
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
        "awaiting_permission" => {
            // Cycle the glyph itself in lock-step with the color pulse — even
            // if the user can't see colors well, the shape change is obvious.
            if (tick_phase / 2) % 2 == 0 {
                "★"
            } else {
                "✦"
            }
        }
        "exited" => "○",
        "resume_failed" => "✗",
        _ => "?",
    }
}

fn draw_card(
    f: &mut ratatui::Frame,
    s: &SessionInfo,
    selected: bool,
    tick_phase: u32,
    area: Rect,
    flashing: bool,
) {
    let theme = crate::theme::current();
    let color = status_color_pulsed(&s.status, tick_phase);
    let awaiting = (s.status == "awaiting_permission") || flashing;
    let border_type = if selected { BorderType::Double } else { BorderType::Rounded };
    let border_style = if selected {
        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
    } else if awaiting {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.dim)
    };

    let glyph = status_glyph(&s.status, tick_phase);
    let (title_label, title_is_auto) = match s.display_override.as_deref().or(s.ai_title.as_deref()) {
        Some(t) if !t.is_empty() => (t.to_string(), true),
        _ => (s.name.clone(), false),
    };
    let title_style = if title_is_auto {
        Style::default().fg(theme.title).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.title_fallback).add_modifier(Modifier::ITALIC)
    };

    use ratatui::text::{Line, Span};

    let status_label = display_status(&s.status);
    let time_ago = format_time_ago(s.last_activity_ms);
    let model = s.model.as_deref().map(short_model).unwrap_or("—");

    // Title spans the top border. Compact: glyph · title · status · time-ago · model (· !)
    let dangerous = s.extra_args.iter().any(|a| a == DANGEROUS_FLAG);
    let mut title_spans = vec![
        Span::styled(format!(" {glyph}  "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(title_label, title_style),
        Span::styled("  ·  ", Style::default().fg(theme.dim)),
        Span::styled(status_label, Style::default().fg(color)),
        Span::styled(format!("  ·  {time_ago}"), Style::default().fg(theme.dim)),
        Span::styled(format!("  ·  {model}"), Style::default().fg(theme.model)),
    ];
    if dangerous {
        title_spans.push(Span::styled(
            "  · !".to_string(),
            Style::default().fg(theme.context_high).add_modifier(Modifier::BOLD),
        ));
    }
    title_spans.push(Span::raw(" "));
    let title_line = Line::from(title_spans);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(border_type)
        .border_style(border_style)
        .title(title_line);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Subtle awaiting bg fill inside the card body — same convention as sidebar.
    if awaiting {
        for r in 0..inner.height {
            f.render_widget(
                Paragraph::new(" ".repeat(inner.width as usize))
                    .style(Style::default().bg(theme.awaiting_bg)),
                Rect::new(inner.x, inner.y + r, inner.width, 1),
            );
        }
    }

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
    let mut cwd_style = Style::default().fg(theme.cwd);
    if awaiting { cwd_style = cwd_style.bg(theme.awaiting_bg); }
    f.render_widget(
        Paragraph::new(cwd_short).style(cwd_style),
        chunks[1],
    );

    let preview = match s.last_message.as_deref() {
        Some(m) if !m.is_empty() => truncate_ellipsis(m, chunks[2].width as usize),
        _ => "(no messages yet)".to_string(),
    };
    let mut preview_style = if s.last_message.is_some() {
        Style::default().fg(theme.body).add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC)
    };
    if awaiting { preview_style = preview_style.bg(theme.awaiting_bg); }
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
    let bg = if awaiting { Some(theme.awaiting_bg) } else { None };
    let style_with_bg = |fg: Color| -> Style {
        let mut st = Style::default().fg(fg);
        if let Some(b) = bg { st = st.bg(b); }
        st
    };
    let stats_line = Line::from(vec![
        Span::styled(format!("{}t", s.turn_count), style_with_bg(theme.body)),
        Span::styled(
            format!("  ·  {} → {}", compact_num(s.tokens_input), compact_num(s.tokens_output)),
            style_with_bg(theme.dim),
        ),
        Span::styled(
            format!("  ·  {}", format_cost(cost)),
            style_with_bg(theme.cost).add_modifier(Modifier::BOLD),
        ),
        Span::styled(tool_part, style_with_bg(theme.tool)),
        Span::styled(exit_part, style_with_bg(theme.dim)),
    ]);
    f.render_widget(Paragraph::new(stats_line), chunks[3]);
}

const CARD_W: u16 = 38;
const CARD_H: u16 = 7;

fn draw_grid(
    f: &mut ratatui::Frame,
    sessions: &[SessionInfo],
    selected: usize,
    tick_phase: u32,
    area: Rect,
    last_status: &std::collections::HashMap<Uuid, (String, SystemTime)>,
) -> u16 {
    let cols = ((area.width + 1) / (CARD_W + 1)).max(1);
    let row_stride = CARD_H + 1;
    let visible_rows = (area.height / row_stride).max(1) as usize;

    let selected_row = selected / cols as usize;
    let scroll_row_off = selected_row.saturating_sub(visible_rows.saturating_sub(1));

    for (idx, s) in sessions.iter().enumerate() {
        let row = idx / cols as usize;
        if row < scroll_row_off {
            continue;
        }
        let render_row = (row - scroll_row_off) as u16;
        if (render_row as usize) >= visible_rows {
            break;
        }
        let col = (idx % cols as usize) as u16;
        let x = area.x + col * (CARD_W + 1);
        let y = area.y + render_row * row_stride;
        let max_w = (area.x + area.width).saturating_sub(x);
        let card_w = CARD_W.min(max_w);
        if card_w < 12 || y + CARD_H > area.y + area.height {
            continue;
        }
        let flashing = last_status
            .get(&s.id)
            .and_then(|(_, t)| t.elapsed().ok())
            .map(|d| d.as_millis())
            .map(|ms| ms <= 500)
            .unwrap_or(false);
        draw_card(
            f,
            s,
            idx == selected,
            tick_phase,
            Rect::new(x, y, card_w, CARD_H),
            flashing,
        );
    }

    cols
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
        "resume_failed" => "resume failed",
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

    // Header — glyph, name, status, time-ago, model, ctx %
    let theme = crate::theme::current();
    let info = app.sessions.iter().find(|s| s.id == session_id);
    use ratatui::text::{Line, Span};
    let header_line = if let Some(s) = info {
        let color = status_color(&s.status);
        let glyph = status_glyph(&s.status, app.tick_phase);
        let (name, name_is_auto) = match s.display_override.as_deref().or(s.ai_title.as_deref()) {
            Some(t) if !t.is_empty() => (t.to_string(), true),
            _ => (s.name.clone(), false),
        };
        let name_style = if name_is_auto {
            Style::default().fg(theme.title).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.title_fallback).add_modifier(Modifier::ITALIC)
        };
        let model = s.model.as_deref().map(short_model).unwrap_or("");
        let time_ago = format_time_ago(s.last_activity_ms);
        let mut spans = vec![
            Span::styled(format!(" {glyph}  "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(name, name_style),
            Span::styled(format!("  ·  {}", display_status(&s.status)), Style::default().fg(color)),
            Span::styled(format!("  ·  {time_ago}"), Style::default().fg(theme.dim)),
            Span::styled(format!("  ·  {model}"), Style::default().fg(theme.model)),
        ];
        if let Some(pct) = s.context_pct {
            let bar_color = match pct {
                0..=59 => theme.context_low,
                60..=84 => theme.context_mid,
                _ => theme.context_high,
            };
            spans.push(Span::styled("  ·  ctx ", Style::default().fg(theme.dim)));
            spans.push(Span::styled(
                format!("{pct}%"),
                Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
            ));
        }
        if s.extra_args.iter().any(|a| a == DANGEROUS_FLAG) {
            spans.push(Span::styled(
                "  ·  !".to_string(),
                Style::default().fg(theme.context_high).add_modifier(Modifier::BOLD),
            ));
        }
        Line::from(spans)
    } else {
        Line::from(Span::styled(format!(" attached — {session_id} "), Style::default().fg(theme.accent)))
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
        theme.footer_scroll
    } else if prefix_active {
        theme.footer_prefix
    } else {
        theme.dim
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
