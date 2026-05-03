//! Daemon PID file. Lets `claws kill-server --force` bypass the auth-protected
//! shutdown RPC and PID-kill the daemon directly when the auth dance is stuck.
//!
//! The state directory is restricted to the current user by OS permissions
//! (Unix mode 0700 dir for the socket; Windows %LOCALAPPDATA% default ACL),
//! so only the user who owns the daemon can read its PID and kill it.

use crate::paths;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

const PID_FILENAME: &str = "daemon.pid";

pub fn path() -> Result<PathBuf> {
    Ok(paths::state_dir()?.join(PID_FILENAME))
}

pub fn write_self() -> Result<()> {
    let p = path()?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let pid = std::process::id();
    std::fs::write(&p, pid.to_string()).context("write daemon.pid")?;
    Ok(())
}

pub fn remove() {
    if let Ok(p) = path() {
        let _ = std::fs::remove_file(p);
    }
}

/// Read the recorded PID and PID-kill that process. Returns:
/// - `Ok(true)` if a daemon was killed
/// - `Ok(false)` if no PID file existed or the recorded PID was already gone
/// - `Err(_)` if we found a PID but couldn't kill it for some other reason
pub fn force_kill() -> Result<bool> {
    let p = path()?;
    let pid_str = match std::fs::read_to_string(&p) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    let pid: u32 = pid_str
        .trim()
        .parse()
        .with_context(|| format!("malformed pid file: {}", p.display()))?;

    #[cfg(unix)]
    {
        let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                // Process is already gone; clean up the stale pid file.
                let _ = std::fs::remove_file(&p);
                return Ok(false);
            }
            anyhow::bail!("kill {pid}: {err}");
        }
    }

    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("invoke taskkill")?;
        if !status.success() {
            // ERROR_NOT_FOUND or similar — process already gone.
            let _ = std::fs::remove_file(&p);
            return Ok(false);
        }
    }

    let _ = std::fs::remove_file(&p);
    Ok(true)
}
