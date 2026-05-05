use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::PathBuf;

fn dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "claws").context("could not resolve project directories")
}

pub fn state_dir() -> Result<PathBuf> {
    let p = dirs()?.data_local_dir().to_path_buf();
    std::fs::create_dir_all(&p).ok();
    Ok(p)
}

pub fn log_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("claws.log"))
}

pub fn config_file() -> Result<PathBuf> {
    let p = dirs()?.config_dir().to_path_buf();
    std::fs::create_dir_all(&p).ok();
    Ok(p.join("config.toml"))
}

#[cfg(unix)]
pub fn socket_name() -> Result<String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    // Use /tmp/claws-<uid>/sock — same pattern as tmux. /tmp persists across
    // SSH logouts (until reboot), so resuming a session after logging back in
    // works reliably. XDG_RUNTIME_DIR (the conventional home for runtime
    // sockets) is wiped by systemd-logind when the user has zero active
    // sessions, which would orphan a still-running daemon between SSH
    // disconnects.
    let uid = unsafe { libc::getuid() };
    let dir = PathBuf::from(format!("/tmp/claws-{uid}"));
    std::fs::create_dir_all(&dir).ok();
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let p = dir.join("sock");
    Ok(String::from_utf8_lossy(p.as_os_str().as_bytes()).into_owned())
}

#[cfg(windows)]
pub fn socket_name() -> Result<String> {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
    Ok(format!(r"\\.\pipe\claws-{user}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_name_has_expected_shape() {
        let s = socket_name().expect("socket name");
        #[cfg(unix)]
        {
            assert!(s.starts_with("/tmp/claws-"), "unexpected unix socket: {s}");
            assert!(s.ends_with("/sock"), "unexpected unix socket: {s}");
        }
        #[cfg(windows)]
        {
            assert!(s.starts_with(r"\\.\pipe\claws-"), "unexpected windows pipe: {s}");
        }
    }

    #[test]
    fn log_file_under_state_dir() {
        let s = state_dir().expect("state dir");
        let l = log_file().expect("log file");
        assert!(l.starts_with(&s));
        assert_eq!(l.file_name().and_then(|s| s.to_str()), Some("claws.log"));
    }
}
