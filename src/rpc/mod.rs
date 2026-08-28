pub mod methods;
pub mod wire;

use crate::state::AppState;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Starts the local JSON-RPC server.
///
/// What it listens on is the one operating-system-shaped decision in the
/// daemon and lives entirely in `crate::ipc` - a Unix domain socket in a
/// private directory, or a named pipe, depending. Nothing below this line
/// knows or cares which.
pub async fn start(state: AppState, socket_path: PathBuf) -> Result<()> {
    let mut listener = crate::ipc::listen(&socket_path)
        .await
        .with_context(|| format!("listening on {}", socket_path.display()))?;
    tracing::info!("listening on {}", socket_path.display());

    loop {
        let stream = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(state, stream).await {
                tracing::debug!("connection closed: {e}");
            }
        });
    }
}

async fn handle_connection(state: AppState, stream: crate::ipc::Conn) -> Result<()> {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();
    let mut subscriptions: HashSet<String> = HashSet::new();
    let mut events = state.events.subscribe();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break }; // EOF: client disconnected
                if line.is_empty() {
                    continue;
                }
                let response = handle_line(&state, &line, &mut subscriptions).await;
                write_half.write_all(response.as_bytes()).await?;
                write_half.write_all(b"\n").await?;
            }
            event = events.recv() => {
                let event = match event {
                    Ok(e) => e,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                // "message"/"presenceChange" only go to subscribed clients;
                // everything else broadcasts to everyone (matches api.c's
                // api_event_sink scoping rule exactly).
                if event.is_scoped() {
                    match event.buffer_id() {
                        Some(id) if subscriptions.contains(id) => {}
                        _ => continue,
                    }
                }
                let line = serde_json::json!({ "event": event.name, "data": event.data }).to_string();
                write_half.write_all(line.as_bytes()).await?;
                write_half.write_all(b"\n").await?;
            }
        }
    }
    Ok(())
}

async fn handle_line(state: &AppState, line: &str, subscriptions: &mut HashSet<String>) -> String {
    let parsed: Result<wire::Request, _> = serde_json::from_str(line);
    let request = match parsed {
        Ok(r) => r,
        Err(e) => return wire::err_response(&serde_json::Value::Null, &e.to_string()).to_string(),
    };

    let (result, error) = methods::dispatch(state, &request.method, &request.params, subscriptions).await;
    match error {
        Some(msg) => wire::err_response(&request.id, &msg).to_string(),
        None => wire::ok_response(&request.id, result.unwrap_or(serde_json::Value::Null)).to_string(),
    }
}

pub fn default_socket_path() -> PathBuf {
    crate::ipc::default_endpoint()
}
