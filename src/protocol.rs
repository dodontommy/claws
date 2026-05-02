use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
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
