//! Per-daemon-startup auth token.
//!
//! Lives at `state_dir/auth.token`. The daemon writes a fresh random token
//! every time it starts; clients (TUI, hook-emit, CLI subcommands) read it
//! and attach it to every RPC request. The daemon rejects requests with the
//! wrong token, which closes the Windows named-pipe ACL gap (default ACL on
//! `\\.\pipe\claws-{user}` lets any local account connect — without auth,
//! that account could ask our daemon to spawn `claude` with arbitrary args
//! as the daemon's user).
//!
//! On Unix the file is written with mode 0o600. On Windows the default ACL
//! on `%LOCALAPPDATA%\claws\` already restricts read access to the current
//! user, so the file inherits that.

use crate::paths;
use anyhow::{Context, Result};
use std::path::PathBuf;
use uuid::Uuid;

const TOKEN_FILENAME: &str = "auth.token";

pub fn token_path() -> Result<PathBuf> {
    Ok(paths::state_dir()?.join(TOKEN_FILENAME))
}

/// Generate a fresh 256-bit token and write it atomically to the token file.
/// Called once by the daemon at startup, before it begins accepting clients.
pub fn write_new_token() -> Result<String> {
    write_new_token_at(&token_path()?)
}

/// Read the token file. Returns Err if missing or unreadable.
pub fn read_token() -> Result<String> {
    read_token_at(&token_path()?)
}

pub fn write_new_token_at(path: &std::path::Path) -> Result<String> {
    let token = format!(
        "{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple(),
    );
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let tmp = path.with_extension("token.tmp");
    std::fs::write(&tmp, token.as_bytes()).context("write auth.token tmp")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }

    // On Windows, std::fs::rename fails if the destination exists. Remove
    // first, then rename. (On Unix rename atomically replaces.)
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&tmp, path).context("rename auth.token")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(token)
}

pub fn read_token_at(path: &std::path::Path) -> Result<String> {
    let s = std::fs::read_to_string(path).context("read auth.token")?;
    Ok(s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("claws-test-{}-{}", name, Uuid::new_v4().simple()));
        p
    }

    #[test]
    fn round_trip() {
        let p = unique_tmp("auth").join("auth.token");
        let written = write_new_token_at(&p).expect("write token");
        let read = read_token_at(&p).expect("read token");
        assert_eq!(written, read);
        assert!(written.len() >= 32, "token unexpectedly short: {}", written.len());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn each_call_generates_a_new_token() {
        let p = unique_tmp("auth-rotate").join("auth.token");
        let a = write_new_token_at(&p).unwrap();
        let b = write_new_token_at(&p).unwrap();
        assert_ne!(a, b);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_missing_is_err() {
        let p = unique_tmp("auth-missing").join("auth.token");
        assert!(read_token_at(&p).is_err());
    }
}
