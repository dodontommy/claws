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
        let cmd = format!("\"{exe_str}\" hook-emit --session {session_id} --event {ev}");
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

pub fn write_settings_for(session_id: Uuid) -> Result<PathBuf> {
    let dir = paths::state_dir()?
        .join("sessions")
        .join(session_id.to_string());
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("settings.json");
    let exe = std::env::current_exe()?;
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
