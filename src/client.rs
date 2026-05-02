use crate::paths;
use crate::protocol::{Request, Response, SessionInfo};
use crate::spawn;
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ToFsName};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub async fn ping() -> Result<()> {
    print_resp(&call_with_autospawn("ping", json!(null)).await?);
    Ok(())
}

pub async fn kill_server() -> Result<()> {
    let resp = match call_no_spawn("shutdown", json!(null)).await {
        Ok(r) => r,
        Err(_) => {
            println!("(no daemon running)");
            return Ok(());
        }
    };
    print_resp(&resp);
    Ok(())
}

pub async fn create_session(cwd: String, name: Option<String>, model: Option<String>) -> Result<()> {
    let params = json!({"cwd": cwd, "name": name, "model": model});
    print_resp(&call_with_autospawn("create_session", params).await?);
    Ok(())
}

pub async fn list_sessions() -> Result<()> {
    print_resp(&call_with_autospawn("list_sessions", json!(null)).await?);
    Ok(())
}

pub async fn send_input(session_id: String, data: String) -> Result<()> {
    let bytes = data.replace("\\n", "\n").replace("\\r", "\r").replace("\\t", "\t");
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes.as_bytes());
    let params = json!({"session_id": session_id, "data_b64": b64});
    print_resp(&call_with_autospawn("send_input", params).await?);
    Ok(())
}

pub async fn read_output(session_id: String, since: u64) -> Result<()> {
    let params = json!({"session_id": session_id, "since": since});
    let resp = call_with_autospawn("read_output", params).await?;
    if let Some(result) = &resp.result {
        let data_b64 = result.get("data_b64").and_then(|v| v.as_str()).unwrap_or("");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .unwrap_or_default();
        let next_seq = result.get("next_seq").and_then(|v| v.as_u64()).unwrap_or(0);
        let status = result.get("status").and_then(|v| v.as_str()).unwrap_or("?");
        eprintln!("[{} bytes, next_seq={}, status={}]", bytes.len(), next_seq, status);
        use std::io::Write;
        std::io::stdout().write_all(&bytes).ok();
        std::io::stdout().flush().ok();
    } else if let Some(e) = &resp.error {
        eprintln!("error {}: {}", e.code, e.message);
    }
    Ok(())
}

pub async fn close_session(session_id: String) -> Result<()> {
    let params = json!({"session_id": session_id});
    print_resp(&call_with_autospawn("close_session", params).await?);
    Ok(())
}

// Non-printing variants used by the TUI. They return parsed values instead
// of pretty-printing to stdout.

pub async fn list_sessions_raw() -> Result<Vec<SessionInfo>> {
    let resp = call_with_autospawn("list_sessions", json!(null)).await?;
    let val = resp.result.ok_or_else(|| anyhow!("daemon returned no result"))?;
    Ok(serde_json::from_value(val)?)
}

pub async fn create_session_raw(
    cwd: String,
    name: Option<String>,
    model: Option<String>,
) -> Result<SessionInfo> {
    let params = json!({"cwd": cwd, "name": name, "model": model});
    let resp = call_with_autospawn("create_session", params).await?;
    if let Some(e) = resp.error {
        return Err(anyhow!("{}: {}", e.code, e.message));
    }
    let val = resp.result.ok_or_else(|| anyhow!("daemon returned no result"))?;
    Ok(serde_json::from_value(val)?)
}

pub async fn close_session_raw(session_id: String) -> Result<()> {
    let params = json!({"session_id": session_id});
    let resp = call_with_autospawn("close_session", params).await?;
    if let Some(e) = resp.error {
        return Err(anyhow!("{}: {}", e.code, e.message));
    }
    Ok(())
}

fn print_resp(resp: &Response) {
    match (&resp.result, &resp.error) {
        (Some(v), _) => println!("{}", serde_json::to_string_pretty(v).unwrap_or_default()),
        (_, Some(e)) => eprintln!("error {}: {}", e.code, e.message),
        _ => eprintln!("(empty response)"),
    }
}

async fn call_with_autospawn(method: &str, params: Value) -> Result<Response> {
    let stream = connect_or_spawn().await?;
    rpc_round_trip(stream, method, params).await
}

pub(crate) async fn call_no_spawn(method: &str, params: Value) -> Result<Response> {
    let stream = connect_once().await?;
    rpc_round_trip(stream, method, params).await
}

/// Like `call_no_spawn` but eats all errors. Used for hook-emit, where we
/// must never break the user's claude session because the daemon is down.
pub(crate) async fn call_no_spawn_silent(method: &str, params: Value) -> Result<()> {
    if let Ok(stream) = connect_once().await {
        let _ = rpc_round_trip(stream, method, params).await;
    }
    Ok(())
}

async fn connect_once() -> Result<interprocess::local_socket::tokio::Stream> {
    let sock = paths::socket_name()?;
    let name = sock
        .to_fs_name::<GenericFilePath>()
        .context("invalid socket name")?;
    interprocess::local_socket::tokio::Stream::connect(name)
        .await
        .context("could not connect to daemon")
}

async fn connect_or_spawn() -> Result<interprocess::local_socket::tokio::Stream> {
    let sock = paths::socket_name()?;
    let make_name = || {
        sock.clone()
            .to_fs_name::<GenericFilePath>()
            .context("invalid socket name")
    };

    if let Ok(s) = interprocess::local_socket::tokio::Stream::connect(make_name()?).await {
        return Ok(s);
    }

    tracing::info!("daemon not reachable, auto-spawning");
    spawn::spawn_detached_daemon()?;

    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(s) = interprocess::local_socket::tokio::Stream::connect(make_name()?).await {
            return Ok(s);
        }
    }

    anyhow::bail!("daemon spawned but socket did not become reachable within 2s")
}

async fn rpc_round_trip(
    stream: interprocess::local_socket::tokio::Stream,
    method: &str,
    params: Value,
) -> Result<Response> {
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
    let req = Request {
        id: 1,
        method: method.to_string(),
        params,
    };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).await?;
    let resp: Response = serde_json::from_str(resp_line.trim_end())
        .context("daemon sent malformed response")?;
    Ok(resp)
}
