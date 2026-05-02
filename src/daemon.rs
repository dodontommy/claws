use crate::hook;
use crate::paths;
use crate::protocol::*;
use crate::registry::SessionRegistry;
use crate::session::{spawn_session, Session};
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

    let name = sock
        .clone()
        .to_fs_name::<GenericFilePath>()
        .context("invalid socket name")?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .context("failed to bind socket (already in use?)")?;
    tracing::info!("daemon listening");

    let shutdown = Arc::new(Notify::new());
    let registry = SessionRegistry::new();

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
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, reg, sd).await {
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

    Ok(())
}

async fn handle_client(
    stream: interprocess::local_socket::tokio::Stream,
    registry: SessionRegistry,
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
        let resp = dispatch(&req, &registry, &shutdown).await;
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        writer.write_all(out.as_bytes()).await?;
        writer.flush().await?;
    }
}

async fn dispatch(
    req: &Request,
    registry: &SessionRegistry,
    shutdown: &Notify,
) -> Response {
    match req.method.as_str() {
        "ping" => ok(req.id, json!("pong")),
        "shutdown" => {
            shutdown.notify_waiters();
            ok(req.id, json!("shutting down"))
        }
        "create_session" => match serde_json::from_value::<CreateSessionParams>(req.params.clone()) {
            Ok(p) => handle_create(req.id, p, registry).await,
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
            Ok(p) => handle_close(req.id, p, registry).await,
            Err(e) => err(req.id, RpcError::invalid_params(e.to_string())),
        },
        "hook_event" => match serde_json::from_value::<HookEventParams>(req.params.clone()) {
            Ok(p) => handle_hook_event(req.id, p, registry).await,
            Err(e) => err(req.id, RpcError::invalid_params(e.to_string())),
        },
        other => err(req.id, RpcError::method_not_found(other)),
    }
}

async fn handle_create(id: u64, p: CreateSessionParams, reg: &SessionRegistry) -> Response {
    tracing::info!(cwd = %p.cwd, name = ?p.name, model = ?p.model, "create_session: begin");
    let cwd = PathBuf::from(&p.cwd);
    if !cwd.is_dir() {
        return err(
            id,
            RpcError::invalid_params(format!("cwd does not exist or is not a directory: {}", p.cwd)),
        );
    }

    // Pre-generate the session UUID so we can write the per-session settings.json
    // and pass `--session-id <uuid>` and `--settings <path>` together.
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
    let session = match tokio::task::spawn_blocking(move || {
        spawn_session(session_id, cwd_clone, name, model, settings_path)
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
    reg.insert(session);
    ok(id, serde_json::to_value(info).unwrap())
}

async fn handle_list(id: u64, reg: &SessionRegistry) -> Response {
    let mut infos: Vec<SessionInfo> = reg.all().iter().map(|s| session_info(s)).collect();
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

async fn handle_close(id: u64, p: SessionIdParam, reg: &SessionRegistry) -> Response {
    match reg.get(p.session_id) {
        Some(s) => {
            s.close();
            ok(id, json!({"closed": true}))
        }
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
