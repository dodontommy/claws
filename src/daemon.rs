use crate::hook;
use crate::paths;
use crate::persist::Store;
use crate::protocol::*;
use crate::registry::SessionRegistry;
use crate::session::{spawn_session, Session, SpawnMode};
use anyhow::{Context, Result};
use base64::Engine;
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;
use uuid::Uuid;

pub async fn run() -> Result<()> {
    let sock = paths::socket_name()?;
    tracing::info!(socket = %sock, "daemon starting");

    #[cfg(unix)]
    let _ = std::fs::remove_file(&sock);

    // Bind the socket FIRST so a "second daemon trying to start" loses on
    // bind and exits without ever touching the token file. Writing the token
    // before binding (the previous order) was a race: two near-simultaneous
    // starts had the second daemon overwrite the first's token on disk
    // before failing to bind, leaving every subsequent client request
    // rejected by the running daemon as -32001 unauthorized.
    let name = sock
        .clone()
        .to_fs_name::<GenericFilePath>()
        .context("invalid socket name")?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .context("failed to bind socket (already in use?)")?;

    let auth_token = Arc::new(crate::auth::write_new_token().context("write auth token")?);
    tracing::info!("auth token written");
    if let Err(e) = crate::pidfile::write_self() {
        tracing::warn!(error = %e, "could not write daemon.pid (force-kill won't work)");
    }
    tracing::info!("daemon listening");

    let shutdown = Arc::new(Notify::new());
    let registry = SessionRegistry::new();
    let store = Store::open()?;

    auto_resume(&store, &registry).await;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => {
                tracing::info!("daemon shutting down, closing all sessions");
                registry.close_all();
                break;
            }
            res = listener.accept() => {
                match res {
                    Ok(stream) => {
                        let sd = shutdown.clone();
                        let reg = registry.clone();
                        let st = store.clone();
                        let tok = auth_token.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, tok, reg, st, sd).await {
                                tracing::warn!(error = %e, "client connection closed");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    let _ = std::fs::remove_file(&sock);
    crate::pidfile::remove();

    Ok(())
}

async fn auto_resume(store: &Store, reg: &SessionRegistry) {
    let to_resume = match store.list_resumable() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "could not list resumable sessions");
            return;
        }
    };
    if to_resume.is_empty() {
        return;
    }
    tracing::info!(count = to_resume.len(), "resuming sessions");
    for ps in to_resume {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        if !ps.cwd.is_dir() {
            tracing::warn!(id = %ps.id, cwd = %ps.cwd.display(), "skipping resume: cwd missing");
            reg.record_resume_failure(crate::registry::FailedResume {
                id: ps.id,
                cwd: ps.cwd.clone(),
                name: ps.name.clone(),
                display_override: ps.display_override.clone(),
                failed_at_ms: now_ms,
                reason: format!("cwd missing: {}", ps.cwd.display()),
            });
            continue;
        }
        let settings_path = match hook::write_settings_for(ps.id) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(error = %e, "failed to write hook settings on resume");
                None
            }
        };
        let id = ps.id;
        let cwd = ps.cwd.clone();
        let name = Some(ps.name.clone());
        let model = ps.model.clone();
        let extra_args = ps.extra_args.clone();
        let result = tokio::task::spawn_blocking(move || {
            spawn_session(id, cwd, name, model, settings_path, SpawnMode::Resume, extra_args)
        })
        .await;
        match result {
            Ok(Ok(s)) => {
                if let Some(name) = ps.display_override.clone() {
                    s.set_display_override(Some(name));
                }
                tracing::info!(id = %s.id, cwd = %s.cwd.display(), "resumed");
                reg.insert(s);
            }
            Ok(Err(e)) => {
                tracing::warn!(id = %ps.id, error = ?e, "resume failed");
                reg.record_resume_failure(crate::registry::FailedResume {
                    id: ps.id,
                    cwd: ps.cwd.clone(),
                    name: ps.name.clone(),
                    display_override: ps.display_override.clone(),
                    failed_at_ms: now_ms,
                    reason: format!("{e:#}"),
                });
            }
            Err(e) => {
                tracing::warn!(id = %ps.id, error = %e, "resume join error");
                reg.record_resume_failure(crate::registry::FailedResume {
                    id: ps.id,
                    cwd: ps.cwd.clone(),
                    name: ps.name.clone(),
                    display_override: ps.display_override.clone(),
                    failed_at_ms: now_ms,
                    reason: format!("join error: {e}"),
                });
            }
        }
    }
}

async fn handle_client(
    stream: interprocess::local_socket::tokio::Stream,
    auth_token: Arc<String>,
    registry: SessionRegistry,
    store: Store,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            tracing::debug!("client disconnected");
            return Ok(());
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, line = %trimmed, "malformed request");
                continue;
            }
        };
        let resp = dispatch(&req, &auth_token, &registry, &store, &shutdown).await;
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        writer.write_all(out.as_bytes()).await?;
        writer.flush().await?;
    }
}

async fn dispatch(
    req: &Request,
    auth_token: &str,
    registry: &SessionRegistry,
    store: &Store,
    shutdown: &Notify,
) -> Response {
    // Constant-time-ish equality on the auth token. UUID tokens are short
    // and the secret comparison isn't truly time-constant in pure Rust,
    // but the attacker can't observe timing on a local IPC socket from
    // a different user account anyway.
    if req.auth.is_empty() || req.auth != auth_token {
        tracing::warn!(method = %req.method, "rejected request: bad or missing auth");
        return err(req.id, RpcError::unauthorized());
    }
    let server_v = env!("CARGO_PKG_VERSION");
    if !req.claws_version.is_empty() && req.claws_version != server_v {
        tracing::warn!(
            client = %req.claws_version,
            daemon = %server_v,
            "client/daemon version mismatch"
        );
    }
    match req.method.as_str() {
        "ping" => ok(req.id, json!("pong")),
        "shutdown" => {
            shutdown.notify_waiters();
            ok(req.id, json!("shutting down"))
        }
        "create_session" => match serde_json::from_value::<CreateSessionParams>(req.params.clone()) {
            Ok(p) => handle_create(req.id, p, registry, store).await,
            Err(e) => err(req.id, RpcError::invalid_params(e.to_string())),
        },
        "list_sessions" => handle_list(req.id, registry).await,
        "send_input" => match serde_json::from_value::<SendInputParams>(req.params.clone()) {
            Ok(p) => handle_send_input(req.id, p, registry).await,
            Err(e) => err(req.id, RpcError::invalid_params(e.to_string())),
        },
        "read_output" => match serde_json::from_value::<ReadOutputParams>(req.params.clone()) {
            Ok(p) => handle_read_output(req.id, p, registry).await,
            Err(e) => err(req.id, RpcError::invalid_params(e.to_string())),
        },
        "close_session" => match serde_json::from_value::<SessionIdParam>(req.params.clone()) {
            Ok(p) => handle_close(req.id, p, registry, store).await,
            Err(e) => err(req.id, RpcError::invalid_params(e.to_string())),
        },
        "hook_event" => match serde_json::from_value::<HookEventParams>(req.params.clone()) {
            Ok(p) => handle_hook_event(req.id, p, registry).await,
            Err(e) => err(req.id, RpcError::invalid_params(e.to_string())),
        },
        "resize_session" => match serde_json::from_value::<ResizeParams>(req.params.clone()) {
            Ok(p) => handle_resize(req.id, p, registry).await,
            Err(e) => err(req.id, RpcError::invalid_params(e.to_string())),
        },
        "rename_session" => match serde_json::from_value::<RenameParams>(req.params.clone()) {
            Ok(p) => handle_rename(req.id, p, registry, store).await,
            Err(e) => err(req.id, RpcError::invalid_params(e.to_string())),
        },
        "restart_session" => match serde_json::from_value::<SessionIdParam>(req.params.clone()) {
            Ok(p) => handle_restart(req.id, p, registry, store).await,
            Err(e) => err(req.id, RpcError::invalid_params(e.to_string())),
        },
        other => err(req.id, RpcError::method_not_found(other)),
    }
}

async fn handle_create(
    id: u64,
    p: CreateSessionParams,
    reg: &SessionRegistry,
    store: &Store,
) -> Response {
    tracing::info!(cwd = %p.cwd, name = ?p.name, model = ?p.model, "create_session: begin");
    let cwd = PathBuf::from(&p.cwd);
    if !cwd.is_dir() {
        return err(
            id,
            RpcError::invalid_params(format!("cwd does not exist or is not a directory: {}", p.cwd)),
        );
    }

    let session_id = Uuid::new_v4();
    let settings_path = match hook::write_settings_for(session_id) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "failed to write hook settings; spawning without hooks");
            None
        }
    };

    let cwd_clone = cwd.clone();
    let name = p.name.clone();
    let model = p.model.clone();
    let extra_args = p.extra_args.clone();
    let extra_args_for_spawn = extra_args.clone();
    let session = match tokio::task::spawn_blocking(move || {
        spawn_session(
            session_id,
            cwd_clone,
            name,
            model,
            settings_path,
            SpawnMode::Fresh,
            extra_args_for_spawn,
        )
    })
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::error!(error = ?e, "spawn_session failed");
            return err(id, RpcError::internal(format!("spawn failed: {e:#}")));
        }
        Err(e) => return err(id, RpcError::internal(format!("join failed: {e}"))),
    };
    tracing::info!(session_id = %session.id, "create_session: spawn returned");
    let info = session_info(&session);

    if let Err(e) = store.insert(
        session.id,
        &session.cwd.to_string_lossy(),
        &session.name,
        session.model_requested.as_deref(),
        info.started_at_ms,
        &extra_args,
    ) {
        tracing::warn!(error = %e, "failed to persist session metadata");
    }

    reg.insert(session);
    ok(id, serde_json::to_value(info).unwrap())
}

async fn handle_list(id: u64, reg: &SessionRegistry) -> Response {
    let mut infos: Vec<SessionInfo> = reg.all().iter().map(|s| session_info(s)).collect();
    // Tack on synthetic entries for resume failures so the user sees them
    // in the dashboard instead of silently losing the session.
    for fr in reg.failed_resumes() {
        infos.push(SessionInfo {
            id: fr.id,
            name: fr.name,
            cwd: fr.cwd.to_string_lossy().into_owned(),
            status: "resume_failed".to_string(),
            exit_code: None,
            started_at_ms: fr.failed_at_ms,
            last_activity_ms: fr.failed_at_ms,
            model: None,
            current_tool: None,
            last_message: Some(format!("resume failed: {}", fr.reason)),
            ai_title: None,
            turn_count: 0,
            tokens_input: 0,
            tokens_output: 0,
            tokens_cache_read: 0,
            display_override: fr.display_override,
            context_pct: None,
            context_used: None,
            context_total: None,
            extra_args: Vec::new(),
        });
    }
    infos.sort_by_key(|i| i.started_at_ms);
    ok(id, serde_json::to_value(infos).unwrap())
}

async fn handle_send_input(id: u64, p: SendInputParams, reg: &SessionRegistry) -> Response {
    let s = match reg.get(p.session_id) {
        Some(s) => s,
        None => return err(id, RpcError::session_not_found(p.session_id)),
    };
    let data = match base64::engine::general_purpose::STANDARD.decode(&p.data_b64) {
        Ok(d) => d,
        Err(e) => return err(id, RpcError::invalid_params(format!("bad base64: {e}"))),
    };
    if let Err(e) = s.send_input(&data) {
        return err(id, RpcError::internal(format!("write failed: {e}")));
    }
    ok(id, json!({"bytes": data.len()}))
}

async fn handle_read_output(id: u64, p: ReadOutputParams, reg: &SessionRegistry) -> Response {
    let s = match reg.get(p.session_id) {
        Some(s) => s,
        None => return err(id, RpcError::session_not_found(p.session_id)),
    };
    let (bytes, next) = s.read_output(p.since);
    let status = s.status();
    let result = ReadOutputResult {
        data_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        next_seq: next,
        status: status.label().to_string(),
        exit_code: status.exit_code(),
    };
    ok(id, serde_json::to_value(result).unwrap())
}

async fn handle_close(
    id: u64,
    p: SessionIdParam,
    reg: &SessionRegistry,
    store: &Store,
) -> Response {
    // Mark closed_by_user in the store BEFORE removing from registry — this
    // prevents auto-resume from bringing back a session the user explicitly
    // asked to delete. Sessions that exit on their own (claude crash, /quit,
    // daemon restart) keep closed_by_user=0 and DO get resumed next time.
    if let Err(e) = store.mark_closed_by_user(p.session_id) {
        tracing::warn!(error = %e, "could not mark session closed_by_user");
    }
    if let Some(s) = reg.remove(p.session_id) {
        s.close();
        return ok(id, json!({"closed": true}));
    }
    // No live session; might be a synthetic resume_failed row. Drop it and
    // call this success — the user just wants the entry to go away.
    if reg.forget_failed_resume(p.session_id) {
        return ok(id, json!({"closed": true}));
    }
    err(id, RpcError::session_not_found(p.session_id))
}

async fn handle_rename(
    id: u64,
    p: RenameParams,
    reg: &SessionRegistry,
    store: &Store,
) -> Response {
    let s = match reg.get(p.session_id) {
        Some(s) => s,
        None => return err(id, RpcError::session_not_found(p.session_id)),
    };
    let trimmed = p.name.trim();
    let new = if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };
    s.set_display_override(new.clone());
    if let Err(e) = store.set_display_override(p.session_id, new.as_deref()) {
        tracing::warn!(error = %e, "could not persist rename");
    }
    ok(id, json!({"renamed": true}))
}

async fn handle_restart(
    id: u64,
    p: SessionIdParam,
    reg: &SessionRegistry,
    store: &Store,
) -> Response {
    let old = match reg.remove(p.session_id) {
        Some(s) => s,
        None => return err(id, RpcError::session_not_found(p.session_id)),
    };
    let cwd = old.cwd.clone();
    let name = old.name.clone();
    let model = old.model_requested.clone();
    let display_override = old.display_override();
    // We need to re-load extra_args from the store (the live Session doesn't
    // remember them — they're only used at spawn). Easier: just resume with no
    // extra args. If the user wanted them, they're in the persisted row.
    let extra_args: Vec<String> = store
        .list_resumable()
        .ok()
        .and_then(|v| v.into_iter().find(|r| r.id == p.session_id))
        .map(|r| r.extra_args)
        .unwrap_or_default();

    old.close();
    // Brief pause so claude releases the JSONL lock before --resume.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let settings_path = match hook::write_settings_for(p.session_id) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "settings write on restart");
            None
        }
    };
    let session_id = p.session_id;
    let result = tokio::task::spawn_blocking(move || {
        spawn_session(session_id, cwd, Some(name), model, settings_path, SpawnMode::Resume, extra_args)
    })
    .await;
    match result {
        Ok(Ok(s)) => {
            if let Some(name) = display_override {
                s.set_display_override(Some(name));
            }
            reg.insert(s);
            ok(id, json!({"restarted": true}))
        }
        Ok(Err(e)) => err(id, RpcError::internal(format!("restart spawn failed: {e:#}"))),
        Err(e) => err(id, RpcError::internal(format!("restart join failed: {e}"))),
    }
}

async fn handle_resize(id: u64, p: ResizeParams, reg: &SessionRegistry) -> Response {
    match reg.get(p.session_id) {
        Some(s) => match s.resize(p.rows, p.cols) {
            Ok(()) => ok(id, json!({"rows": p.rows, "cols": p.cols})),
            Err(e) => err(id, RpcError::internal(format!("resize failed: {e}"))),
        },
        None => err(id, RpcError::session_not_found(p.session_id)),
    }
}

async fn handle_hook_event(id: u64, p: HookEventParams, reg: &SessionRegistry) -> Response {
    match reg.get(p.session_id) {
        Some(s) => {
            tracing::debug!(session_id = %p.session_id, event = %p.event, "hook event");
            s.on_hook_event(&p.event, &p.payload);
            ok(id, json!("ok"))
        }
        None => {
            tracing::debug!(session_id = %p.session_id, event = %p.event, "hook for unknown session");
            err(id, RpcError::session_not_found(p.session_id))
        }
    }
}

fn session_info(s: &Session) -> SessionInfo {
    let snap = s.snapshot();
    let scraped = s.context_status();
    // Prefer scraped context (always reflects what Claude actually shows).
    // If the user's statusLine doesn't expose context, fall back to a
    // computed estimate from the latest assistant message's prompt token
    // count divided by the model's known context limit.
    let (context_pct, context_used, context_total) = if let Some(c) = scraped {
        (Some(c.pct), Some(c.used), Some(c.total))
    } else if let Some(model) = snap.model.as_deref() {
        let limit = model_context_limit(model);
        let used = snap.latest_input_tokens + snap.latest_cache_read_input_tokens;
        if limit > 0 && used > 0 {
            let pct = ((used.saturating_mul(100)) / limit).min(100) as u8;
            (
                Some(pct),
                Some(compact_tokens(used)),
                Some(compact_tokens(limit)),
            )
        } else {
            (None, None, None)
        }
    } else {
        (None, None, None)
    };
    SessionInfo {
        id: s.id,
        name: s.name.clone(),
        cwd: s.cwd.to_string_lossy().into_owned(),
        status: snap.status.label().to_string(),
        exit_code: snap.status.exit_code(),
        started_at_ms: ms_since_epoch(s.started_at),
        last_activity_ms: ms_since_epoch(snap.last_activity),
        model: snap.model,
        current_tool: snap.current_tool,
        last_message: snap.last_message,
        ai_title: snap.ai_title,
        turn_count: snap.turn_count,
        tokens_input: snap.tokens_input,
        tokens_output: snap.tokens_output,
        tokens_cache_read: snap.tokens_cache_read,
        display_override: s.display_override(),
        context_pct,
        context_used,
        context_total,
        extra_args: s.extra_args.clone(),
    }
}

/// Public list-price context window per model family. Best-effort heuristic
/// for the computed-fallback context bar.
fn model_context_limit(model: &str) -> u64 {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        1_000_000
    } else if m.contains("sonnet") {
        1_000_000
    } else if m.contains("haiku") {
        200_000
    } else {
        200_000
    }
}

fn compact_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{}k", n / 1000)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn ms_since_epoch(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn ok(id: u64, value: serde_json::Value) -> Response {
    Response { id, result: Some(value), error: None }
}

fn err(id: u64, e: RpcError) -> Response {
    Response { id, result: None, error: Some(e) }
}
