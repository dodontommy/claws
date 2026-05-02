use crate::ring::RingBuffer;
use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::SystemTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnMode {
    /// New conversation: pass `--session-id <uuid>` to claude.
    Fresh,
    /// Resume an existing transcript: pass `--resume <uuid>` to claude.
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Spawning,
    Idle,
    Streaming,
    AwaitingPermission,
    Exited(i32),
}

impl SessionStatus {
    pub fn label(&self) -> &'static str {
        match self {
            SessionStatus::Spawning => "spawning",
            SessionStatus::Idle => "idle",
            SessionStatus::Streaming => "streaming",
            SessionStatus::AwaitingPermission => "awaiting_permission",
            SessionStatus::Exited(_) => "exited",
        }
    }
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            SessionStatus::Exited(c) => Some(*c),
            _ => None,
        }
    }
}

pub struct Session {
    pub id: Uuid,
    pub name: String,
    pub cwd: PathBuf,
    pub started_at: SystemTime,
    pub model_requested: Option<String>,
    /// Extra args this session was spawned with (verbatim). Surfaced in the
    /// detail/details views and used to flag dangerous flags in the sidebar.
    pub extra_args: Vec<String>,
    state: Arc<Mutex<SessionRuntime>>,
    display_override: Mutex<Option<String>>,
}

pub struct SessionSnapshot {
    pub status: SessionStatus,
    pub last_activity: SystemTime,
    pub current_tool: Option<String>,
    pub last_message: Option<String>,
    pub ai_title: Option<String>,
    pub model: Option<String>,
    pub turn_count: u32,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub latest_input_tokens: u64,
    pub latest_cache_read_input_tokens: u64,
}

struct SessionRuntime {
    status: SessionStatus,
    last_activity: SystemTime,
    ring: RingBuffer,
    /// Held to keep the slave end of the PTY alive and to support resize later.
    _master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    kill_tx: Option<mpsc::Sender<()>>,

    /// Daemon-side vt100 parser fed from the same reader as the ring buffer.
    /// Used to extract Claude's status-bar info (context %, cost, reset time)
    /// that aren't reported via the JSONL transcript.
    parser: vt100::Parser,

    current_tool: Option<String>,
    last_message: Option<String>,
    ai_title: Option<String>,
    model_actual: Option<String>,
    turn_count: u32,
    tokens_input: u64,
    tokens_output: u64,
    tokens_cache_read: u64,

    /// Most-recent assistant message's prompt-side token counts. Used to
    /// estimate context-window fill when Claude's status bar isn't scraped
    /// (custom statusLine or no rendered status line yet).
    latest_input_tokens: u64,
    latest_cache_read_input_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct ContextStatus {
    pub pct: u8,
    pub used: String,
    pub total: String,
}

const RING_CAP: usize = 1024 * 1024;

pub fn spawn_session(
    id: Uuid,
    cwd: PathBuf,
    name: Option<String>,
    model: Option<String>,
    settings_path: Option<PathBuf>,
    mode: SpawnMode,
    extra_args: Vec<String>,
) -> Result<Session> {
    let display_name = name.unwrap_or_else(|| {
        cwd.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string()
    });

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .context("openpty failed")?;

    let mut cmd = CommandBuilder::new("claude");
    match mode {
        SpawnMode::Fresh => {
            cmd.arg("--session-id");
            cmd.arg(id.to_string());
        }
        SpawnMode::Resume => {
            cmd.arg("--resume");
            cmd.arg(id.to_string());
        }
    }
    if let Some(m) = &model {
        cmd.arg("--model");
        cmd.arg(m);
    }
    if let Some(s) = &settings_path {
        cmd.arg("--settings");
        cmd.arg(s.to_string_lossy().as_ref());
    }
    for a in &extra_args {
        cmd.arg(a);
    }
    cmd.cwd(&cwd);
    for (k, v) in std::env::vars() {
        cmd.env(k, v);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .context("failed to spawn claude")?;
    drop(pair.slave);

    let reader = pair.master.try_clone_reader().context("clone PTY reader")?;
    let writer = pair.master.take_writer().context("take PTY writer")?;

    let (kill_tx, kill_rx) = mpsc::channel::<()>();
    let runtime = Arc::new(Mutex::new(SessionRuntime {
        status: SessionStatus::Spawning,
        last_activity: SystemTime::now(),
        ring: RingBuffer::new(RING_CAP),
        _master: pair.master,
        writer,
        kill_tx: Some(kill_tx),
        parser: vt100::Parser::new(24, 80, 0),
        current_tool: None,
        last_message: None,
        ai_title: None,
        model_actual: None,
        turn_count: 0,
        tokens_input: 0,
        tokens_output: 0,
        tokens_cache_read: 0,
        latest_input_tokens: 0,
        latest_cache_read_input_tokens: 0,
    }));

    {
        let runtime = runtime.clone();
        std::thread::spawn(move || pty_reader_loop(runtime, reader));
    }
    {
        let runtime = runtime.clone();
        std::thread::spawn(move || pty_waiter_loop(runtime, child, kill_rx));
    }
    if let Ok(jsonl) = jsonl_path_for(&cwd, id) {
        let runtime = runtime.clone();
        std::thread::spawn(move || jsonl_tail_loop(runtime, jsonl));
    }

    Ok(Session {
        id,
        name: display_name,
        cwd,
        started_at: SystemTime::now(),
        model_requested: model,
        extra_args,
        state: runtime,
        display_override: Mutex::new(None),
    })
}

fn pty_reader_loop(
    runtime: Arc<Mutex<SessionRuntime>>,
    mut reader: Box<dyn std::io::Read + Send>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut s = runtime.lock().unwrap();
                s.ring.append(&buf[..n]);
                s.parser.process(&buf[..n]);
                s.last_activity = SystemTime::now();
            }
            Err(e) => {
                tracing::debug!(error = %e, "pty reader closing");
                break;
            }
        }
    }
}

fn pty_waiter_loop(
    runtime: Arc<Mutex<SessionRuntime>>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    kill_rx: mpsc::Receiver<()>,
) {
    loop {
        if let Ok(()) = kill_rx.try_recv() {
            let _ = child.kill();
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut s = runtime.lock().unwrap();
                s.status = SessionStatus::Exited(status.exit_code() as i32);
                s.kill_tx = None;
                return;
            }
            Ok(None) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                tracing::warn!(error = %e, "child wait error");
                let mut s = runtime.lock().unwrap();
                s.status = SessionStatus::Exited(-1);
                s.kill_tx = None;
                return;
            }
        }
    }
}

fn jsonl_path_for(cwd: &Path, session_id: Uuid) -> Result<PathBuf> {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME"))?;
    let cwd_str = cwd.to_string_lossy();
    let slug: String = cwd_str
        .chars()
        .map(|c| match c {
            ':' | '/' | '\\' => '-',
            x => x,
        })
        .collect();
    Ok(PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(slug)
        .join(format!("{session_id}.jsonl")))
}

fn jsonl_tail_loop(runtime: Arc<Mutex<SessionRuntime>>, path: PathBuf) {
    use std::fs::File;

    // Wait for file to appear (claude takes a beat to create it)
    let mut waited_ms = 0u64;
    while !path.exists() {
        if matches!(runtime.lock().unwrap().status, SessionStatus::Exited(_)) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        waited_ms += 100;
        if waited_ms > 60_000 {
            tracing::warn!(path = %path.display(), "JSONL never appeared, abandoning tail");
            return;
        }
    }

    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "failed to open JSONL for tail");
            return;
        }
    };
    let mut pos: u64 = 0;
    let mut leftover = String::new();

    loop {
        let exited = matches!(runtime.lock().unwrap().status, SessionStatus::Exited(_));
        let _ = drain_and_process(&mut file, &mut pos, &mut leftover, &runtime);
        if exited {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn drain_and_process(
    file: &mut std::fs::File,
    pos: &mut u64,
    leftover: &mut String,
    runtime: &Arc<Mutex<SessionRuntime>>,
) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(*pos))?;
    let mut new_bytes = Vec::new();
    file.read_to_end(&mut new_bytes)?;
    if new_bytes.is_empty() {
        return Ok(());
    }
    *pos += new_bytes.len() as u64;
    let text = match std::str::from_utf8(&new_bytes) {
        Ok(s) => s.to_string(),
        Err(_) => return Ok(()),
    };
    let combined: String = leftover.clone() + &text;
    let mut last_nl = 0;
    for (i, ch) in combined.char_indices() {
        if ch == '\n' {
            let line = &combined[last_nl..i];
            if !line.is_empty() {
                process_jsonl_line(runtime, line);
            }
            last_nl = i + 1;
        }
    }
    *leftover = combined[last_nl..].to_string();
    Ok(())
}

fn parse_context_from_line(line: &str) -> Option<ContextStatus> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    for i in 0..tokens.len() {
        let t = tokens[i];
        let pct_str = match t.strip_suffix('%') {
            Some(p) => p,
            None => continue,
        };
        let pct: u8 = match pct_str.parse() {
            Ok(p) if p <= 100 => p,
            _ => continue,
        };
        let next = match tokens.get(i + 1) {
            Some(n) => n,
            None => continue,
        };
        let (used, total) = match next.split_once('/') {
            Some(t) => t,
            None => continue,
        };
        // Sanity: total must contain at least one digit (filters out "v100%"
        // false positives where the next token is something unrelated).
        if !total.chars().any(|c| c.is_ascii_digit()) {
            continue;
        }
        return Some(ContextStatus {
            pct,
            used: used.to_string(),
            total: total.to_string(),
        });
    }
    None
}

fn process_jsonl_line(runtime: &Arc<Mutex<SessionRuntime>>, line: &str) {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };
    let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let mut s = runtime.lock().unwrap();
    match event_type {
        "ai-title" => {
            if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                s.ai_title = Some(t.to_string());
            }
        }
        "user" => {
            // The "user" type message corresponds to a user prompt turn.
            // Filter out queue-operation entries which are also typed but lack a
            // proper `message` body.
            if v.get("message").is_some() {
                s.turn_count = s.turn_count.saturating_add(1);
            }
        }
        "assistant" => {
            let msg = v.get("message");
            if let Some(arr) = msg.and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                let texts: Vec<&str> = arr
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect();
                let joined = texts.join(" ");
                if !joined.is_empty() {
                    // Keep up to ~2000 chars so the detail pane can show the
                    // full message on a tall terminal. The TUI truncates with
                    // an ellipsis if it doesn't fit anyway.
                    s.last_message = Some(joined.chars().take(2000).collect());
                }
            }
            if let Some(model) = msg.and_then(|m| m.get("model")).and_then(|m| m.as_str()) {
                s.model_actual = Some(model.to_string());
            }
            if let Some(usage) = msg.and_then(|m| m.get("usage")) {
                if let Some(t) = usage.get("input_tokens").and_then(|t| t.as_u64()) {
                    s.tokens_input += t;
                    s.latest_input_tokens = t;
                }
                if let Some(t) = usage.get("output_tokens").and_then(|t| t.as_u64()) {
                    s.tokens_output += t;
                }
                if let Some(t) = usage.get("cache_read_input_tokens").and_then(|t| t.as_u64()) {
                    s.tokens_cache_read += t;
                    s.latest_cache_read_input_tokens = t;
                }
            }
        }
        _ => {}
    }
}

impl Session {
    pub fn send_input(&self, data: &[u8]) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        std::io::Write::write_all(&mut s.writer, data)?;
        std::io::Write::flush(&mut s.writer)?;
        s.last_activity = SystemTime::now();
        Ok(())
    }

    pub fn read_output(&self, since: u64) -> (Vec<u8>, u64) {
        let s = self.state.lock().unwrap();
        s.ring.read_from(since)
    }

    pub fn status(&self) -> SessionStatus {
        self.state.lock().unwrap().status
    }

    pub fn close(&self) {
        let mut s = self.state.lock().unwrap();
        if let Some(tx) = s.kill_tx.take() {
            let _ = tx.send(());
        }
    }

    pub fn display_override(&self) -> Option<String> {
        self.display_override.lock().unwrap().clone()
    }

    pub fn set_display_override(&self, name: Option<String>) {
        *self.display_override.lock().unwrap() = name;
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        s._master
            .resize(portable_pty::PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| anyhow::anyhow!("pty resize failed: {e}"))?;
        s.parser.set_size(rows, cols);
        Ok(())
    }

    /// Scrape the daemon-side vt100 screen for Claude's context-fill status,
    /// which appears in its bottom status bar as e.g. `12% 120k/1.0M`. Not
    /// available in the JSONL or hooks — only on screen.
    pub fn context_status(&self) -> Option<ContextStatus> {
        let s = self.state.lock().unwrap();
        let screen = s.parser.screen();
        let (rows, cols) = screen.size();
        // Walk bottom-up; the status bar is at the bottom of Claude's UI.
        for r in (0..rows).rev() {
            let mut line = String::new();
            for c in 0..cols {
                if let Some(cell) = screen.cell(r, c) {
                    let contents = cell.contents();
                    if contents.is_empty() {
                        line.push(' ');
                    } else {
                        line.push_str(&contents);
                    }
                }
            }
            if let Some(ctx) = parse_context_from_line(&line) {
                return Some(ctx);
            }
        }
        None
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        let s = self.state.lock().unwrap();
        SessionSnapshot {
            status: s.status,
            last_activity: s.last_activity,
            current_tool: s.current_tool.clone(),
            last_message: s.last_message.clone(),
            ai_title: s.ai_title.clone(),
            model: s.model_actual.clone().or_else(|| self.model_requested.clone()),
            turn_count: s.turn_count,
            tokens_input: s.tokens_input,
            tokens_output: s.tokens_output,
            tokens_cache_read: s.tokens_cache_read,
            latest_input_tokens: s.latest_input_tokens,
            latest_cache_read_input_tokens: s.latest_cache_read_input_tokens,
        }
    }

    pub fn on_hook_event(&self, event: &str, payload: &Value) {
        let mut s = self.state.lock().unwrap();
        s.last_activity = SystemTime::now();
        match event {
            "SessionStart" => {
                if matches!(s.status, SessionStatus::Spawning) {
                    s.status = SessionStatus::Idle;
                }
            }
            "UserPromptSubmit" => {
                s.status = SessionStatus::Streaming;
            }
            "PreToolUse" => {
                s.status = SessionStatus::Streaming;
                s.current_tool = payload
                    .get("tool_name")
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }
            "PostToolUse" | "PostToolUseFailure" => {
                s.current_tool = None;
            }
            "Notification" | "PermissionRequest" => {
                s.status = SessionStatus::AwaitingPermission;
                if let Some(tool) = payload.get("tool_name").and_then(|v| v.as_str()) {
                    s.current_tool = Some(tool.to_string());
                }
            }
            "Stop" => {
                if !matches!(s.status, SessionStatus::Exited(_)) {
                    s.status = SessionStatus::Idle;
                }
                s.current_tool = None;
            }
            "SessionEnd" => {
                // Wait for waiter task to detect process exit.
            }
            _ => {}
        }
    }
}
