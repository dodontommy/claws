use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    /// Per-daemon-startup auth token (see src/auth.rs). Required on every
    /// request; daemon rejects mismatches. Empty string when missing — also
    /// rejected, which is what we want for "no token file" cases.
    #[serde(default)]
    pub auth: String,
    /// Reporter version. Logged by the daemon on mismatch but not enforced,
    /// so a slightly newer client and slightly older daemon can still talk.
    #[serde(default)]
    pub claws_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Notification {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Response(Response),
    Notification(Notification),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcError {
    pub fn method_not_found(method: &str) -> Self {
        Self { code: -32601, message: format!("method not found: {method}") }
    }
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self { code: -32602, message: msg.into() }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self { code: -32603, message: msg.into() }
    }
    pub fn session_not_found(id: Uuid) -> Self {
        Self { code: -32004, message: format!("session not found: {id}") }
    }
    pub fn unauthorized() -> Self {
        Self { code: -32001, message: "unauthorized: bad or missing auth token".to_string() }
    }
}

// ---- Method-specific param/result types ----

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateSessionParams {
    pub cwd: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Extra args appended verbatim to the `claude` command line. Allows
    /// `--dangerously-skip-permissions`, `--system-prompt "..."`, `--effort xhigh`,
    /// `--add-dir <path>`, etc. Shell-split client-side before sending.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: Uuid,
    pub name: String,
    pub cwd: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub started_at_ms: u128,
    pub last_activity_ms: u128,

    // Tier 2 fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_title: Option<String>,
    pub turn_count: u32,

    // Tier 3 — token usage
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,

    /// User-set display name (via `r` rename). Wins over ai_title and name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_override: Option<String>,

    // Context-window fill, scraped from Claude's status bar (no JSONL/API
    // exposure). All three Some/None together.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_pct: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_used: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_total: Option<String>,

    /// Verbatim flags this session was spawned with (e.g. `--effort xhigh`,
    /// `--dangerously-skip-permissions`). Empty when no extras were supplied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RenameParams {
    pub session_id: Uuid,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionIdParam {
    pub session_id: Uuid,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SendInputParams {
    pub session_id: Uuid,
    pub data_b64: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReadOutputParams {
    pub session_id: Uuid,
    #[serde(default)]
    pub since: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReadOutputResult {
    pub data_b64: String,
    pub next_seq: u64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HookEventParams {
    pub session_id: Uuid,
    pub event: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResizeParams {
    pub session_id: Uuid,
    pub rows: u16,
    pub cols: u16,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trip() {
        let r = Request {
            id: 7,
            method: "ping".into(),
            params: json!({"k": "v"}),
            auth: "abcd".into(),
            claws_version: "0.1.7".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.method, "ping");
        assert_eq!(back.auth, "abcd");
        assert_eq!(back.claws_version, "0.1.7");
        assert_eq!(back.params["k"], "v");
    }

    #[test]
    fn request_legacy_no_auth_field_deserializes() {
        // An older client that doesn't know about auth/version still parses.
        // Daemon will reject it as unauthorized; it shouldn't crash on parse.
        let s = r#"{"id":1,"method":"ping","params":null}"#;
        let r: Request = serde_json::from_str(s).unwrap();
        assert_eq!(r.auth, "");
        assert_eq!(r.claws_version, "");
    }

    #[test]
    fn response_ok_serializes_without_error_field() {
        let r = Response {
            id: 1,
            result: Some(json!("pong")),
            error: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"result\":\"pong\""));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn rpc_error_unauthorized_uses_dedicated_code() {
        let e = RpcError::unauthorized();
        assert_eq!(e.code, -32001);
    }
}
