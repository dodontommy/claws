//! Phone companion: HTTP + WebSocket server that exposes the daemon's session
//! registry to a paired PWA. Lives inside the daemon process so it shares the
//! live `SessionRegistry` directly (no extra IPC hop).
//!
//! Design notes:
//! - Auth is a *device token* per paired device, separate from the daemon's
//!   `auth.token`. Revoking a phone doesn't break the TUI.
//! - Pairing: CLI calls `phone_pair_code` RPC, daemon mints a single-use code
//!   that lives in memory only and expires in 10 minutes. The phone redeems it
//!   at `POST /api/pair` for a long-lived device token.
//! - Bind is restricted to loopback in Phase 1. For cellular reach the user
//!   fronts the listener with `tailscale serve --https` or `cloudflared tunnel`,
//!   which also terminates TLS (PWAs require a secure context). Phase 4 will
//!   add a `--bind` flag with a self-signed cert for users who want raw LAN.
//! - Embedded assets: the PWA shell is committed to `phone-pwa/dist/` and
//!   compiled into the binary via `rust-embed`, so there's no JS toolchain at
//!   `cargo build` time.

use crate::persist::Store;
use crate::registry::SessionRegistry;
use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::Engine;
use rand::Rng;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

#[derive(RustEmbed)]
#[folder = "phone-pwa/dist/"]
struct PhoneAssets;

/// Lives in `state_dir/phone.json`. Holds the listener bind, the persisted
/// list of paired devices, and (Phase 3) VAPID push keys + per-device push
/// subscriptions. We persist the bind so the daemon re-starts the listener on
/// boot if the user enabled it previously.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PhoneState {
    #[serde(default)]
    pub enabled: bool,
    /// Bind address as a string so it round-trips through JSON cleanly. Empty
    /// or absent → default `127.0.0.1:9817`.
    #[serde(default)]
    pub bind: String,
    #[serde(default)]
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: Uuid,
    /// The bearer token the phone presents on every WS/HTTP request. Long
    /// random; revoking a device removes its entry from this list.
    pub token: String,
    /// Optional label so the user can tell their devices apart in
    /// `claws phone devices`.
    #[serde(default)]
    pub label: Option<String>,
    pub paired_at_ms: u128,
    /// Last activity timestamp for staleness display in `phone devices`.
    #[serde(default)]
    pub last_seen_ms: u128,
}

const DEFAULT_BIND: &str = "127.0.0.1:9817";
const PHONE_FILE: &str = "phone.json";
const PAIR_CODE_TTL: Duration = Duration::from_secs(600);
const DEVICE_TOKEN_BYTES: usize = 32;
const PAIR_CODE_BYTES: usize = 6; // 12-hex-char codes — easy enough to type.

pub fn state_path() -> Result<PathBuf> {
    Ok(crate::paths::state_dir()?.join(PHONE_FILE))
}

pub fn load_state() -> PhoneState {
    let path = match state_path() {
        Ok(p) => p,
        Err(_) => return PhoneState::default(),
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return PhoneState::default(),
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save_state(s: &PhoneState) -> Result<()> {
    let path = state_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let json = serde_json::to_vec_pretty(s).context("serialize phone state")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).context("write phone state tmp")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(&path);
    }
    std::fs::rename(&tmp, &path).context("rename phone state")?;
    Ok(())
}

/// Shared state for axum handlers.
pub struct AppState {
    registry: SessionRegistry,
    _store: Store,
    /// In-memory pair codes. Single use, expire in PAIR_CODE_TTL.
    pair_codes: Mutex<HashMap<String, Instant>>,
    /// Persisted devices. We hold an `RwLock` over the whole `PhoneState` so
    /// pairing and revocation can be serialized cleanly with reads.
    state: RwLock<PhoneState>,
    /// Rough rate-limit: at most 20 failed pairings/sec across all callers.
    /// Pair codes are 48-bit anyway, but keep brute-force noise out of logs.
    failed_pair_count: Mutex<(Instant, u32)>,
}

#[derive(Clone)]
pub struct PhoneHandle {
    /// Cancels the running server when dropped.
    shutdown: Arc<tokio::sync::Notify>,
    pub bind: SocketAddr,
    state: Arc<AppState>,
}

impl PhoneHandle {
    pub async fn mint_pair_code(&self) -> String {
        let code = random_pair_code();
        let mut g = self.state.pair_codes.lock().await;
        g.insert(code.clone(), Instant::now());
        // Garbage-collect expired entries opportunistically.
        g.retain(|_, t| t.elapsed() < PAIR_CODE_TTL);
        code
    }

    pub async fn devices(&self) -> Vec<Device> {
        self.state.state.read().await.devices.clone()
    }

    pub async fn revoke(&self, id: Uuid) -> bool {
        let mut s = self.state.state.write().await;
        let before = s.devices.len();
        s.devices.retain(|d| d.id != id);
        let removed = s.devices.len() != before;
        if removed {
            let _ = save_state(&s);
        }
        removed
    }

    pub fn stop(&self) {
        self.shutdown.notify_waiters();
    }
}

/// Boot the phone listener. Returns once the listener is bound.
pub async fn start(
    bind: SocketAddr,
    registry: SessionRegistry,
    store: Store,
) -> Result<PhoneHandle> {
    let persisted = load_state();
    let app_state = Arc::new(AppState {
        registry,
        _store: store,
        pair_codes: Mutex::new(HashMap::new()),
        state: RwLock::new(persisted),
        failed_pair_count: Mutex::new((Instant::now(), 0)),
    });

    let router = Router::new()
        .route("/api/pair", post(handle_pair))
        .route("/api/me", get(handle_me))
        .route("/api/sessions", get(handle_sessions))
        .route("/api/ws", get(handle_ws))
        .fallback(handle_static)
        .with_state(app_state.clone());

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind phone listener {bind}"))?;
    let actual = listener.local_addr().unwrap_or(bind);
    tracing::info!(bind = %actual, "phone listener up");

    let shutdown = Arc::new(tokio::sync::Notify::new());
    let sd2 = shutdown.clone();
    tokio::spawn(async move {
        let res = axum::serve(listener, router)
            .with_graceful_shutdown(async move { sd2.notified().await })
            .await;
        if let Err(e) = res {
            tracing::error!(error = %e, "phone listener exited");
        }
    });

    // Persist enabled+bind so daemon restart re-spins the listener.
    {
        let mut s = app_state.state.write().await;
        s.enabled = true;
        s.bind = actual.to_string();
        let _ = save_state(&s);
    }

    Ok(PhoneHandle {
        shutdown,
        bind: actual,
        state: app_state,
    })
}

pub fn parse_bind(s: &str) -> Result<SocketAddr> {
    let s = if s.is_empty() { DEFAULT_BIND } else { s };
    let addr: SocketAddr = s.parse().with_context(|| format!("invalid bind: {s}"))?;
    if !is_loopback(&addr) {
        anyhow::bail!(
            "phone listener must bind to loopback (127.0.0.1 or ::1) in this version. \
             Front it with `tailscale serve --https` or `cloudflared tunnel` for cellular access."
        );
    }
    Ok(addr)
}

fn is_loopback(a: &SocketAddr) -> bool {
    match a {
        SocketAddr::V4(s) => s.ip().is_loopback(),
        SocketAddr::V6(s) => s.ip().is_loopback(),
    }
}

// ---- Handlers ---------------------------------------------------------------

#[derive(Deserialize)]
struct PairBody {
    code: String,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Serialize)]
struct PairResponse {
    device_id: Uuid,
    device_token: String,
}

async fn handle_pair(
    State(s): State<Arc<AppState>>,
    Json(body): Json<PairBody>,
) -> Response {
    // Cheap rate-limit: count failed redemptions, cool off if a window fills up.
    {
        let mut g = s.failed_pair_count.lock().await;
        if g.0.elapsed() > Duration::from_secs(1) {
            *g = (Instant::now(), 0);
        }
        if g.1 > 20 {
            return (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
        }
    }

    let claimed = {
        let mut g = s.pair_codes.lock().await;
        // Constant-ish-time match: walk all codes regardless of hit. Single-use:
        // remove on success.
        let mut found = None;
        for (k, t) in g.iter() {
            if t.elapsed() < PAIR_CODE_TTL && constant_time_eq(k.as_bytes(), body.code.as_bytes()) {
                found = Some(k.clone());
            }
        }
        if let Some(k) = found.as_ref() {
            g.remove(k);
        }
        found
    };

    if claimed.is_none() {
        let mut g = s.failed_pair_count.lock().await;
        g.1 = g.1.saturating_add(1);
        return (StatusCode::UNAUTHORIZED, "bad or expired pair code").into_response();
    }

    let device = Device {
        id: Uuid::new_v4(),
        token: random_token(DEVICE_TOKEN_BYTES),
        label: body.label,
        paired_at_ms: ms_since_epoch(),
        last_seen_ms: ms_since_epoch(),
    };
    let resp = PairResponse {
        device_id: device.id,
        device_token: device.token.clone(),
    };
    {
        let mut g = s.state.write().await;
        g.devices.push(device);
        if let Err(e) = save_state(&g) {
            tracing::error!(error = %e, "failed to persist new device");
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not persist device").into_response();
        }
    }
    (StatusCode::OK, Json(resp)).into_response()
}

async fn handle_me(
    State(s): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    match authenticate(&s, &headers, None).await {
        Some(d) => Json(json!({
            "device_id": d.id,
            "label": d.label,
            "paired_at_ms": d.paired_at_ms,
        })).into_response(),
        None => unauth(),
    }
}

async fn handle_sessions(
    State(s): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if authenticate(&s, &headers, None).await.is_none() {
        return unauth();
    }
    let infos: Vec<Value> = s
        .registry
        .all()
        .iter()
        .map(|sess| session_view(sess))
        .collect();
    Json(infos).into_response()
}

#[derive(Deserialize)]
struct WsQuery {
    token: Option<String>,
}

async fn handle_ws(
    State(s): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let device = match authenticate(&s, &headers, q.token.as_deref()).await {
        Some(d) => d,
        None => return unauth(),
    };
    ws.on_upgrade(move |socket| handle_ws_socket(s, socket, device.id))
}

async fn handle_ws_socket(state: Arc<AppState>, socket: WebSocket, device_id: Uuid) {
    let (mut tx, mut rx) = socket.split();
    use futures::{SinkExt, StreamExt};

    // Per-connection subscription cursor for the currently-watched session.
    let subscribed: Arc<Mutex<Option<(Uuid, u64)>>> = Arc::new(Mutex::new(None));

    // Snapshot push: every 500ms, send a sessions list. Real implementations
    // would push deltas via a broadcast channel sourced in `Session`; polling is
    // pragmatic for v1 because the snapshot is tiny.
    let state_for_snap = state.clone();
    let subscribed_for_snap = subscribed.clone();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(64);
    let out_tx_snap = out_tx.clone();

    let snap_task = tokio::spawn(async move {
        let mut last_summary: Vec<(Uuid, String, u128)> = Vec::new();
        loop {
            // Sessions snapshot ----------------------------------------------
            let sessions: Vec<Value> = state_for_snap
                .registry
                .all()
                .iter()
                .map(|s| session_view(s))
                .collect();
            // Compare a coarse summary so we don't ship snapshots when nothing
            // visible changed. Status + last-activity captures the things the
            // phone UI actually re-renders for.
            let summary: Vec<(Uuid, String, u128)> = sessions
                .iter()
                .map(|v| {
                    (
                        v.get("id").and_then(|x| x.as_str()).and_then(|s| Uuid::parse_str(s).ok()).unwrap_or_default(),
                        v.get("status").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        v.get("last_activity_ms").and_then(|x| x.as_u64()).unwrap_or(0) as u128,
                    )
                })
                .collect();
            if summary != last_summary {
                last_summary = summary;
                let msg = json!({ "kind": "snapshot", "sessions": sessions }).to_string();
                if out_tx_snap.send(msg).await.is_err() {
                    return;
                }
            }

            // PTY tail for the subscribed session ----------------------------
            let cur = { subscribed_for_snap.lock().await.clone() };
            if let Some((sid, cursor)) = cur {
                if let Some(sess) = state_for_snap.registry.get(sid) {
                    let (bytes, next) = sess.read_output(cursor);
                    if !bytes.is_empty() {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        let msg = json!({
                            "kind": "session.output",
                            "session_id": sid,
                            "data_b64": b64,
                            "next_seq": next,
                        }).to_string();
                        // Update cursor before send to avoid resending on a slow
                        // consumer disconnect.
                        {
                            let mut g = subscribed_for_snap.lock().await;
                            if let Some(c) = g.as_mut() {
                                if c.0 == sid {
                                    c.1 = next;
                                }
                            }
                        }
                        if out_tx_snap.send(msg).await.is_err() {
                            return;
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    });

    // Pump from out_rx → websocket
    let send_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if tx.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    // Read commands from client
    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Text(t) => {
                let parsed: serde_json::Result<Value> = serde_json::from_str(&t);
                let v = match parsed {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match v.get("kind").and_then(|x| x.as_str()).unwrap_or("") {
                    "subscribe" => {
                        let sid = v
                            .get("session_id")
                            .and_then(|x| x.as_str())
                            .and_then(|s| Uuid::parse_str(s).ok());
                        let since = v.get("since").and_then(|x| x.as_u64()).unwrap_or(0);
                        if let Some(sid) = sid {
                            *subscribed.lock().await = Some((sid, since));
                        }
                    }
                    "unsubscribe" => {
                        *subscribed.lock().await = None;
                    }
                    "send_input" => {
                        let sid = v
                            .get("session_id")
                            .and_then(|x| x.as_str())
                            .and_then(|s| Uuid::parse_str(s).ok());
                        let data_b64 = v.get("data_b64").and_then(|x| x.as_str()).unwrap_or("");
                        if let (Some(sid), Ok(bytes)) = (
                            sid,
                            base64::engine::general_purpose::STANDARD.decode(data_b64),
                        ) {
                            if let Some(sess) = state.registry.get(sid) {
                                let _ = sess.send_input(&bytes);
                            }
                        }
                    }
                    "ping" => {
                        // Keep alive: clients may send these during long idle.
                    }
                    _ => {}
                }
                // Touch device last_seen so `phone devices` shows liveness.
                touch_device(&state, device_id).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    snap_task.abort();
    send_task.abort();
}

async fn touch_device(state: &Arc<AppState>, id: Uuid) {
    let mut g = state.state.write().await;
    if let Some(d) = g.devices.iter_mut().find(|d| d.id == id) {
        d.last_seen_ms = ms_since_epoch();
    }
}

async fn handle_static(uri: Uri) -> Response {
    let mut path = uri.path().trim_start_matches('/').to_string();
    if path.is_empty() {
        path = "index.html".to_string();
    }
    if let Some(file) = PhoneAssets::get(&path) {
        let mime = mime_guess::from_path(&path).first_or_octet_stream();
        return ([(header::CONTENT_TYPE, mime.essence_str().to_string())], file.data.into_owned()).into_response();
    }
    // SPA fallback: any unmatched path returns the shell so the PWA can route.
    if let Some(file) = PhoneAssets::get("index.html") {
        return (
            [(header::CONTENT_TYPE, "text/html".to_string())],
            file.data.into_owned(),
        )
            .into_response();
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

// ---- Helpers ----------------------------------------------------------------

async fn authenticate(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
) -> Option<Device> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|s| s.to_string())
        .or_else(|| query_token.map(|s| s.to_string()))?;
    let g = state.state.read().await;
    g.devices
        .iter()
        .find(|d| constant_time_eq(d.token.as_bytes(), bearer.as_bytes()))
        .cloned()
}

fn unauth() -> Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill(buf.as_mut_slice());
    // URL-safe base64, no padding — keeps tokens copyable.
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf)
}

fn random_pair_code() -> String {
    // 12 hex chars (~48 bits). Easy to type, hard to guess in 10 minutes.
    let mut buf = [0u8; PAIR_CODE_BYTES];
    rand::thread_rng().fill(&mut buf);
    let mut s = String::with_capacity(PAIR_CODE_BYTES * 2);
    for b in buf {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn ms_since_epoch() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Session JSON shape sent to the PWA. Mirrors `SessionInfo` but adds an
/// `id_seq` (monotonic per-daemon spawn order) we use as the stable sort key
/// for active sessions, and a `dangerous` flag pulled from `extra_args`.
fn session_view(s: &Arc<crate::session::Session>) -> Value {
    let snap = s.snapshot();
    let dangerous = s
        .extra_args
        .iter()
        .any(|a| a == "--dangerously-skip-permissions");
    json!({
        "id": s.id,
        "name": s.name,
        "cwd": s.cwd.to_string_lossy(),
        "status": snap.status.label(),
        "exit_code": snap.status.exit_code(),
        "model": snap.model,
        "current_tool": snap.current_tool,
        "last_message": snap.last_message,
        "ai_title": snap.ai_title,
        "display_override": s.display_override(),
        "turn_count": snap.turn_count,
        "tokens_input": snap.tokens_input,
        "tokens_output": snap.tokens_output,
        "started_at_ms": ms_since_epoch_st(s.started_at),
        "last_activity_ms": ms_since_epoch_st(snap.last_activity),
        "dangerous": dangerous,
        // The seq we use for stable sort: low 64 bits of the UUID v4. Not
        // monotonic but stable and sufficient to keep two streaming rows from
        // swapping. (The TUI uses session id u64s; here we have UUIDs so we
        // hash them deterministically.)
        "id_seq": uuid_seq(s.id),
    })
}

fn uuid_seq(id: Uuid) -> u64 {
    // First 8 bytes of the UUID, big-endian. Any deterministic mapping works.
    let b = id.as_bytes();
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn ms_since_epoch_st(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bind_rejects_non_loopback() {
        assert!(parse_bind("0.0.0.0:9817").is_err());
        assert!(parse_bind("192.168.1.10:9817").is_err());
    }

    #[test]
    fn parse_bind_accepts_loopback() {
        assert!(parse_bind("127.0.0.1:9817").is_ok());
        assert!(parse_bind("[::1]:9817").is_ok());
        assert_eq!(parse_bind("").unwrap().to_string(), DEFAULT_BIND);
    }

    #[test]
    fn pair_codes_are_hex() {
        let c = random_pair_code();
        assert_eq!(c.len(), PAIR_CODE_BYTES * 2);
        assert!(c.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn ct_eq_is_length_safe() {
        assert!(!constant_time_eq(b"a", b"ab"));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
    }
}
