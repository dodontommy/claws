use crate::paths;
use anyhow::Result;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Hook events we register for each session. Includes both `Notification` and
/// `PermissionRequest` because Claude Code's docs disagree on which is real;
/// whichever fires, we treat the session as awaiting input.
const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "PermissionRequest",
    "Stop",
    "SessionEnd",
];

pub fn build_settings_json(claws_exe: &Path, session_id: Uuid) -> Value {
    let exe_str = claws_exe.to_string_lossy().replace('\\', "/");
    let mut hooks = serde_json::Map::new();
    for ev in EVENTS {
        // Don't wrap the path in double-quotes. Recent Claude Code versions
        // exec the hook command directly (no `sh -c`), which means the whole
        // string becomes argv[0..] — and a quoted path becomes a literal
        // file `"/opt/homebrew/bin/claws"` (with quotes) that doesn't exist.
        // Hooks then silently fail to launch and the daemon never sees an
        // event. Paths with embedded spaces will break here; that's a
        // future problem when someone hits it.
        let cmd = format!("{exe_str} hook-emit --session {session_id} --event {ev}");
        let entry = json!([
            {
                "hooks": [
                    { "type": "command", "command": cmd }
                ]
            }
        ]);
        hooks.insert(ev.to_string(), entry);
    }
    json!({ "hooks": hooks })
}

/// Resolve the running executable's path, stripping the ` (deleted)`
/// suffix Linux appends to `/proc/self/exe` when the binary file has
/// been unlinked (typically because an in-place upgrade replaced it
/// while the daemon was running). Without this, `build_settings_json`
/// bakes a literal `claws (deleted) hook-emit ...` command into every
/// per-session settings.json — Claude shell-parses that, hits the
/// unmatched `(`, and blocks UserPromptSubmit with a hook-syntax error.
fn strip_deleted_suffix(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s).to_string())
}

fn current_exe_clean() -> Result<PathBuf> {
    Ok(strip_deleted_suffix(&std::env::current_exe()?))
}

pub fn write_settings_for(session_id: Uuid) -> Result<PathBuf> {
    let dir = paths::state_dir()?
        .join("sessions")
        .join(session_id.to_string());
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("settings.json");
    let exe = current_exe_clean()?;
    let settings = build_settings_json(&exe, session_id);
    std::fs::write(&path, serde_json::to_vec_pretty(&settings)?)?;
    Ok(path)
}

/// Run as `claws hook-emit --session <uuid> --event <name>`. Reads the hook
/// payload from stdin, forwards to the daemon as a `hook_event` RPC, exits 0.
/// If the daemon isn't reachable, we silently no-op so we don't break Claude.
pub async fn run_hook_emit(session_id: Uuid, event: String) -> Result<()> {
    let mut payload_text = String::new();
    let _ = std::io::stdin().read_to_string(&mut payload_text);
    let payload: Value = serde_json::from_str(&payload_text).unwrap_or(Value::Null);

    let params = json!({
        "session_id": session_id.to_string(),
        "event": event,
        "payload": payload,
    });

    let _ = crate::client::call_no_spawn_silent("hook_event", params).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn settings_json_has_entry_per_event() {
        let exe = PathBuf::from("/usr/local/bin/claws");
        let id = Uuid::new_v4();
        let v = build_settings_json(&exe, id);
        let hooks = v.get("hooks").expect("hooks key").as_object().expect("hooks obj");
        assert_eq!(hooks.len(), EVENTS.len());
        for ev in EVENTS {
            assert!(hooks.contains_key(*ev), "missing event {ev}");
        }
    }

    #[test]
    fn settings_json_command_includes_session_id() {
        let exe = PathBuf::from("/usr/local/bin/claws");
        let id = Uuid::new_v4();
        let v = build_settings_json(&exe, id);
        let serialized = serde_json::to_string(&v).unwrap();
        assert!(serialized.contains(&id.to_string()), "session id not in output");
        assert!(serialized.contains("hook-emit"), "hook-emit not in output");
    }

    #[test]
    fn strip_deleted_suffix_removes_linux_proc_marker() {
        // Linux's /proc/self/exe appends " (deleted)" once the on-disk
        // binary has been unlinked (e.g., after an in-place upgrade).
        // The hook command goes into a shell-parsed string, so the
        // unmatched `(` would otherwise corrupt the command.
        let p = PathBuf::from("/home/u/.cargo/bin/claws (deleted)");
        assert_eq!(strip_deleted_suffix(&p), PathBuf::from("/home/u/.cargo/bin/claws"));
    }

    #[test]
    fn strip_deleted_suffix_passes_through_clean_paths() {
        let p = PathBuf::from("/usr/local/bin/claws");
        assert_eq!(strip_deleted_suffix(&p), p);
    }

    #[test]
    fn strip_deleted_suffix_doesnt_match_substring() {
        // Only strip the trailing literal " (deleted)" — a path that
        // happens to contain that text mid-string stays intact.
        let p = PathBuf::from("/opt/weird (deleted) path/claws");
        assert_eq!(strip_deleted_suffix(&p), p);
    }

    #[test]
    fn settings_json_normalizes_windows_backslashes() {
        let exe = PathBuf::from(r"C:\Program Files\claws\claws.exe");
        let id = Uuid::new_v4();
        let v = build_settings_json(&exe, id);
        let serialized = serde_json::to_string(&v).unwrap();
        // The exe path is rewritten with forward slashes so Claude's hook
        // shell parser doesn't choke on backslashes inside the JSON string.
        assert!(serialized.contains("C:/Program Files/claws/claws.exe"));
        assert!(!serialized.contains(r"C:\\Program Files"));
    }
}
