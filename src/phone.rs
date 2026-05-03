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

/// Lives in `state_dir/phone.json`. Holds the listener bind, paired devices,
/// VAPID push keys, and per-device push subscriptions. We persist the bind so
/// the daemon re-starts the listener on boot if the user enabled it previously.
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
    /// VAPID ECDSA P-256 private key in PEM form. Generated lazily on first
    /// phone-start when push is needed; persisted so push subscriptions
    /// survive restarts. Rotating it invalidates all push subscriptions —
    /// the user would have to re-grant push permission on each device.
    #[serde(default)]
    pub vapid_privkey_pem: Option<String>,
    /// VAPID public key in raw uncompressed SEC1 form, base64-url-no-pad.
    /// This is what the browser wants as `applicationServerKey`.
    #[serde(default)]
    pub vapid_pubkey_b64: Option<String>,
    /// User-facing URL the phone should open — `https://<machine>.<tailnet>.ts.net`
    /// or whatever fronting service is in front of the local listener. Set
    /// once via `claws phone pair --url …` and reused for all future QR
    /// codes. Also auto-detected from `tailscale serve status` when missing.
    #[serde(default)]
    pub public_url: Option<String>,
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
    /// Web Push subscription. Set when the device successfully calls
    /// `/api/push/subscribe`. Cleared if a push attempt returns a 410/404
    /// (subscription expired).
    #[serde(default)]
    pub push_sub: Option<PushSub>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSub {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
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
    store: Store,
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

    pub async fn set_public_url(&self, url: String) {
        let mut g = self.state.state.write().await;
        g.public_url = Some(url);
        let _ = save_state(&g);
    }

    /// Resolve the URL the phone should open. Priority:
    ///   1. The explicit value the user saved with `--url` (in phone.json).
    ///   2. Auto-detection via `tailscale serve status --json`. We look for
    ///      a target matching `http://127.0.0.1:<our-port>` or `localhost:<our-port>`
    ///      and use the corresponding tailnet hostname.
    ///   3. Fallback: `http://<bind>` — only useful for browsers running on
    ///      the same machine as the daemon.
    pub async fn resolve_public_url(&self) -> String {
        if let Some(u) = self.state.state.read().await.public_url.clone() {
            return u;
        }
        if let Some(u) = detect_tailscale_serve_url(self.bind.port()) {
            return u;
        }
        format!("http://{}", self.bind)
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
        store,
        pair_codes: Mutex::new(HashMap::new()),
        state: RwLock::new(persisted),
        failed_pair_count: Mutex::new((Instant::now(), 0)),
    });

    // Generate VAPID keys on first start if missing. Held in state so the
    // background push task and the /api/push/vapid_key endpoint share them.
    {
        let mut s = app_state.state.write().await;
        if s.vapid_privkey_pem.is_none() || s.vapid_pubkey_b64.is_none() {
            match generate_vapid() {
                Ok((priv_pem, pub_b64)) => {
                    s.vapid_privkey_pem = Some(priv_pem);
                    s.vapid_pubkey_b64 = Some(pub_b64);
                    let _ = save_state(&s);
                    tracing::info!("generated VAPID key pair for push notifications");
                }
                Err(e) => tracing::warn!(error = %e, "VAPID key generation failed; push disabled"),
            }
        }
    }

    let router = Router::new()
        .route("/api/pair", post(handle_pair))
        .route("/api/me", get(handle_me))
        .route("/api/sessions", get(handle_sessions).post(handle_create_session))
        .route("/api/sessions/:id/close", post(handle_close_session))
        .route("/api/ws", get(handle_ws))
        .route("/api/push/vapid_key", get(handle_vapid_key))
        .route("/api/push/subscribe", post(handle_push_subscribe))
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

    // Push fan-out task: polls the registry every 500ms, tracks per-session
    // status transitions, and delivers Web Push to every device that has a
    // subscription on file when a session enters `awaiting_permission` or
    // exits. Polling is fine — the registry is in-memory and the loop is
    // cheap. A broadcast-channel architecture is the eventual upgrade if we
    // need <500ms latency or finer event types.
    let push_state = app_state.clone();
    let push_sd = shutdown.clone();
    tokio::spawn(async move { push_fanout_loop(push_state, push_sd).await });

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
        push_sub: None,
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
struct CreateSessionBody {
    cwd: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    extra_args: Vec<String>,
    #[serde(default)]
    name: Option<String>,
    /// If true and the cwd doesn't exist yet, mkdir -p before spawning —
    /// mirrors the TUI spawn modal's "[enter to mkdir -p]" behavior.
    #[serde(default)]
    create_cwd: bool,
}

async fn handle_create_session(
    State(s): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateSessionBody>,
) -> Response {
    if authenticate(&s, &headers, None).await.is_none() {
        return unauth();
    }
    if body.create_cwd {
        let p = std::path::PathBuf::from(&body.cwd);
        if !p.is_dir() {
            if let Err(e) = std::fs::create_dir_all(&p) {
                return (StatusCode::BAD_REQUEST, format!("mkdir failed: {e}")).into_response();
            }
        }
    }
    let params = crate::protocol::CreateSessionParams {
        cwd: body.cwd,
        name: body.name,
        model: body.model,
        extra_args: body.extra_args,
    };
    match crate::daemon::create_session_into_registry(params, &s.registry, &s.store).await {
        Ok(info) => Json(info).into_response(),
        Err(crate::daemon::CreateError::InvalidParams(m)) => {
            (StatusCode::BAD_REQUEST, m).into_response()
        }
        Err(crate::daemon::CreateError::Internal(m)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, m).into_response()
        }
    }
}

async fn handle_close_session(
    State(s): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if authenticate(&s, &headers, None).await.is_none() {
        return unauth();
    }
    let sid = match Uuid::parse_str(&id) {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad session id").into_response(),
    };
    match s.registry.remove(sid) {
        Some(sess) => {
            sess.close();
            // Mark closed-by-user in the persistent store so we don't
            // auto-resume it on the next daemon start. Errors are non-fatal.
            let _ = s.store.mark_closed_by_user(sid);
            (StatusCode::OK, "closed").into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
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
        let mut last_status: HashMap<Uuid, String> = HashMap::new();
        loop {
            // Sessions snapshot ----------------------------------------------
            let session_arcs = state_for_snap.registry.all();
            let sessions: Vec<Value> = session_arcs.iter().map(|s| session_view(s)).collect();
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

            // Permission/exit transitions: surface a structured event so the
            // PWA can pop a focused prompt instead of rebuilding from the
            // snapshot. Push notifications cover the background case; this
            // covers the foreground case (no notification needed when the
            // user already has the app open).
            for s in &session_arcs {
                let snap = s.snapshot();
                let cur = snap.status.label().to_string();
                let prev = last_status.get(&s.id).cloned();
                match (prev.as_deref(), cur.as_str()) {
                    (Some(p), "awaiting_permission") if p != "awaiting_permission" => {
                        let display = s
                            .display_override()
                            .or_else(|| snap.ai_title.clone())
                            .unwrap_or_else(|| s.name.clone());
                        let msg = json!({
                            "kind": "session.permission_request",
                            "session_id": s.id,
                            "name": display,
                            "tool_name": snap.current_tool,
                        }).to_string();
                        if out_tx_snap.send(msg).await.is_err() {
                            return;
                        }
                    }
                    (Some(p), "exited") if p != "exited" => {
                        let msg = json!({
                            "kind": "session.exited",
                            "session_id": s.id,
                            "exit_code": snap.status.exit_code(),
                        }).to_string();
                        if out_tx_snap.send(msg).await.is_err() {
                            return;
                        }
                    }
                    _ => {}
                }
                last_status.insert(s.id, cur);
            }
            let live: std::collections::HashSet<Uuid> = session_arcs.iter().map(|s| s.id).collect();
            last_status.retain(|id, _| live.contains(id));

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

/// Shell out to `tailscale serve status --json` and look for a serve entry
/// pointing at our local bind. Returns the user-visible HTTPS URL when found.
/// Best-effort: we silently return None if tailscale isn't on PATH, the JSON
/// shape has changed, or no serve route matches our port.
fn detect_tailscale_serve_url(local_port: u16) -> Option<String> {
    let out = std::process::Command::new("tailscale")
        .args(["serve", "status", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    // Shape (as of recent tailscale): { "Web": { "<host>:<port>": { "Handlers": { "/": { "Proxy": "http://127.0.0.1:9817" } } } }, ... }
    let web = v.get("Web")?.as_object()?;
    let needle_a = format!("http://127.0.0.1:{local_port}");
    let needle_b = format!("http://localhost:{local_port}");
    for (host_port, entry) in web {
        let handlers = entry.get("Handlers").and_then(|h| h.as_object())?;
        for handler in handlers.values() {
            if let Some(proxy) = handler.get("Proxy").and_then(|p| p.as_str()) {
                if proxy == needle_a || proxy == needle_b {
                    // host_port is "machine.tailnet.ts.net:443" or similar.
                    // Strip :443 since that's HTTPS default; keep other ports.
                    let host = host_port.trim_end_matches(":443");
                    return Some(format!("https://{host}"));
                }
            }
        }
    }
    None
}

// ---- Web Push --------------------------------------------------------------

fn generate_vapid() -> Result<(String, String)> {
    // ES256PublicKey::to_bytes() returns the *compressed* form. Browsers
    // expect the *uncompressed* SEC1 (65 bytes, leading 0x04) for
    // applicationServerKey, which only the inner P256PublicKey exposes —
    // reach it via the ECDSAP256KeyPairLike trait.
    use jwt_simple::algorithms::{ECDSAP256KeyPairLike, ES256KeyPair};
    let kp = ES256KeyPair::generate();
    let priv_pem = kp
        .to_pem()
        .map_err(|e| anyhow::anyhow!("vapid to_pem: {e}"))?;
    let pub_bytes = kp.key_pair().public_key().to_bytes_uncompressed();
    let pub_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&pub_bytes);
    Ok((priv_pem, pub_b64))
}

async fn handle_vapid_key(State(s): State<Arc<AppState>>) -> Response {
    let g = s.state.read().await;
    match g.vapid_pubkey_b64.as_ref() {
        Some(k) => Json(json!({ "application_server_key": k })).into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "vapid not initialized").into_response(),
    }
}

#[derive(Deserialize)]
struct SubscribeBody {
    endpoint: String,
    p256dh: String,
    auth: String,
}

async fn handle_push_subscribe(
    State(s): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SubscribeBody>,
) -> Response {
    let device = match authenticate(&s, &headers, None).await {
        Some(d) => d,
        None => return unauth(),
    };
    let mut g = s.state.write().await;
    if let Some(d) = g.devices.iter_mut().find(|d| d.id == device.id) {
        d.push_sub = Some(PushSub {
            endpoint: body.endpoint,
            p256dh: body.p256dh,
            auth: body.auth,
        });
        d.last_seen_ms = ms_since_epoch();
        if let Err(e) = save_state(&g) {
            tracing::warn!(error = %e, "failed to persist push subscription");
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not persist").into_response();
        }
        (StatusCode::OK, "ok").into_response()
    } else {
        unauth()
    }
}

/// Background loop. Compares current session statuses to the prior tick;
/// any session that newly entered `awaiting_permission` or `exited` triggers
/// a push fan-out to every device with a stored subscription. We never push
/// for streaming/idle transitions — those would be too noisy.
async fn push_fanout_loop(state: Arc<AppState>, shutdown: Arc<tokio::sync::Notify>) {
    let mut last: HashMap<Uuid, String> = HashMap::new();
    loop {
        // Cancellation: bail when the listener shuts down.
        let tick = tokio::time::sleep(Duration::from_millis(500));
        tokio::pin!(tick);
        tokio::select! {
            _ = shutdown.notified() => return,
            _ = &mut tick => {}
        }

        let mut events: Vec<PushEvent> = Vec::new();
        for s in state.registry.all() {
            let snap = s.snapshot();
            let cur = snap.status.label().to_string();
            let prev = last.get(&s.id).cloned();
            let display = s
                .display_override()
                .or(snap.ai_title)
                .unwrap_or_else(|| s.name.clone());
            match (prev.as_deref(), cur.as_str()) {
                (Some(p), "awaiting_permission") if p != "awaiting_permission" => {
                    events.push(PushEvent {
                        kind: "awaiting_permission".into(),
                        session_id: s.id,
                        title: format!("{display} — needs you"),
                        body: snap
                            .current_tool
                            .map(|t| format!("wants to run {t}"))
                            .unwrap_or_else(|| "needs your input".into()),
                    });
                }
                (Some(p), "exited") if p != "exited" => {
                    events.push(PushEvent {
                        kind: "exited".into(),
                        session_id: s.id,
                        title: format!("{display} — exited"),
                        body: snap
                            .status
                            .exit_code()
                            .map(|c| format!("exit {c}"))
                            .unwrap_or_else(|| "session ended".into()),
                    });
                }
                _ => {}
            }
            last.insert(s.id, cur);
        }
        // Drop entries for sessions that no longer exist.
        let live: std::collections::HashSet<Uuid> = state.registry.all().iter().map(|s| s.id).collect();
        last.retain(|id, _| live.contains(id));

        if events.is_empty() {
            continue;
        }
        deliver_push_events(&state, events).await;
    }
}

struct PushEvent {
    kind: String,
    session_id: Uuid,
    title: String,
    body: String,
}

async fn deliver_push_events(state: &Arc<AppState>, events: Vec<PushEvent>) {
    use web_push::{
        ContentEncoding, HyperWebPushClient, SubscriptionInfo, VapidSignatureBuilder,
        WebPushClient, WebPushError, WebPushMessageBuilder,
    };

    let (priv_pem, devices) = {
        let g = state.state.read().await;
        let priv_pem = match g.vapid_privkey_pem.clone() {
            Some(p) => p,
            None => return,
        };
        let devices: Vec<Device> = g
            .devices
            .iter()
            .filter(|d| d.push_sub.is_some())
            .cloned()
            .collect();
        (priv_pem, devices)
    };
    if devices.is_empty() {
        return;
    }
    let client = HyperWebPushClient::new();

    // Subscriptions that came back 410/404: drop them on persistence so we
    // don't keep retrying dead endpoints. Collected by id so we can edit
    // the persistent state once at the end.
    let mut expired: Vec<Uuid> = Vec::new();

    for ev in events {
        for d in &devices {
            let sub = match &d.push_sub {
                Some(s) => s,
                None => continue,
            };
            let sub_info = SubscriptionInfo::new(&sub.endpoint, &sub.p256dh, &sub.auth);
            let sig = match VapidSignatureBuilder::from_pem(priv_pem.as_bytes(), &sub_info) {
                Ok(b) => match b.build() {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "vapid sig build failed");
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, "vapid sig from_pem failed");
                    continue;
                }
            };

            let payload = serde_json::to_vec(&json!({
                "kind": ev.kind,
                "session_id": ev.session_id.to_string(),
                "title": ev.title,
                "body": ev.body,
            }))
            .unwrap_or_default();

            let mut msg_b = WebPushMessageBuilder::new(&sub_info);
            msg_b.set_payload(ContentEncoding::Aes128Gcm, &payload);
            msg_b.set_vapid_signature(sig);
            // 4 hour TTL — long enough to survive a phone in airplane mode
            // for a few hours, short enough that we don't pile up notices.
            msg_b.set_ttl(4 * 60 * 60);

            let msg = match msg_b.build() {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "web push build failed");
                    continue;
                }
            };
            match client.send(msg).await {
                Ok(_) => {}
                Err(WebPushError::EndpointNotFound) | Err(WebPushError::EndpointNotValid) => {
                    tracing::info!(device = %d.id, "push subscription expired; dropping");
                    expired.push(d.id);
                }
                Err(e) => tracing::warn!(error = %e, device = %d.id, "push delivery failed"),
            }
        }
    }

    if !expired.is_empty() {
        let mut g = state.state.write().await;
        for id in expired {
            if let Some(dev) = g.devices.iter_mut().find(|d| d.id == id) {
                dev.push_sub = None;
            }
        }
        let _ = save_state(&g);
    }
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
