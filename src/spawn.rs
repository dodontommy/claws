use anyhow::{Context, Result};
use std::process::{Command, Stdio};

/// Spawn `claws daemon` as a detached child that survives the parent.
///
/// - Windows: `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, stdio nulled.
/// - Unix: `setsid()` to leave the controlling terminal, stdio nulled.
pub fn spawn_detached_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("could not resolve current_exe")?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    cmd.spawn().context("failed to spawn detached daemon")?;
    Ok(())
}
