use crate::ring::RingBuffer;
use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::SystemTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Running,
    Exited(i32),
}

impl SessionStatus {
    pub fn label(&self) -> &'static str {
        match self {
            SessionStatus::Running => "running",
            SessionStatus::Exited(_) => "exited",
        }
    }

    pub fn exit_code(&self) -> Option<i32> {
        match self {
            SessionStatus::Running => None,
            SessionStatus::Exited(c) => Some(*c),
        }
    }
}

pub struct Session {
    pub id: Uuid,
    pub name: String,
    pub cwd: PathBuf,
    pub started_at: SystemTime,
    pub model: Option<String>,
    state: Arc<Mutex<SessionRuntime>>,
}

struct SessionRuntime {
    status: SessionStatus,
    last_activity: SystemTime,
    ring: RingBuffer,
    /// Held to keep the slave end of the PTY alive and to support resize later.
    _master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    kill_tx: Option<mpsc::Sender<()>>,
}

const RING_CAP: usize = 1024 * 1024;

pub fn spawn_session(
    cwd: PathBuf,
    name: Option<String>,
    model: Option<String>,
) -> Result<Session> {
    let id = Uuid::new_v4();
    let display_name = name.unwrap_or_else(|| {
        cwd.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string()
    });

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty failed")?;

    let mut cmd = CommandBuilder::new("claude");
    cmd.arg("--session-id");
    cmd.arg(id.to_string());
    if let Some(m) = &model {
        cmd.arg("--model");
        cmd.arg(m);
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

    let reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone PTY reader")?;
    let writer = pair
        .master
        .take_writer()
        .context("failed to take PTY writer")?;

    let (kill_tx, kill_rx) = mpsc::channel::<()>();
    let runtime = Arc::new(Mutex::new(SessionRuntime {
        status: SessionStatus::Running,
        last_activity: SystemTime::now(),
        ring: RingBuffer::new(RING_CAP),
        _master: pair.master,
        writer,
        kill_tx: Some(kill_tx),
    }));

    {
        let runtime = runtime.clone();
        std::thread::spawn(move || pty_reader_loop(runtime, reader));
    }
    {
        let runtime = runtime.clone();
        std::thread::spawn(move || pty_waiter_loop(runtime, child, kill_rx));
    }

    Ok(Session {
        id,
        name: display_name,
        cwd,
        started_at: SystemTime::now(),
        model,
        state: runtime,
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

    pub fn last_activity(&self) -> SystemTime {
        self.state.lock().unwrap().last_activity
    }

    pub fn close(&self) {
        let mut s = self.state.lock().unwrap();
        if let Some(tx) = s.kill_tx.take() {
            let _ = tx.send(());
        }
    }
}
