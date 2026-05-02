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

#[cfg(unix)]
pub fn socket_name() -> Result<String> {
    use std::os::unix::ffi::OsStrExt;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir());
    let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
    let p = runtime.join(format!("claws-{user}.sock"));
    Ok(String::from_utf8_lossy(p.as_os_str().as_bytes()).into_owned())
}

#[cfg(windows)]
pub fn socket_name() -> Result<String> {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
    Ok(format!(r"\\.\pipe\claws-{user}"))
}
