pub mod methods;
pub mod wire;

use crate::state::AppState;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use futures::FutureExt;
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

/// What one client is subscribed to, shared with the tasks answering its
/// requests.
///
/// Shared rather than owned by the connection loop because requests are
/// answered concurrently now: `subscribe` writes this from inside a task, and
/// the event arm reads it from the loop. A std mutex, deliberately - it is
/// held for a set insert and never across an await.
pub type Subscriptions = std::sync::Arc<std::sync::Mutex<HashSet<String>>>;

/// How many requests from one client may be in flight at once.
///
/// A limit rather than none: a request is a spawned task, and a client that
/// asked for ten thousand things at once should wait rather than make the
/// daemon hold ten thousand futures. Well above anything a window does - a
/// busy startup is a few dozen - so in practice this only ever catches a bug.
const MAX_CONCURRENT_REQUESTS: usize = 32;

/// One client, whose requests are answered concurrently.
///
/// They used to be answered one at a time: the loop awaited each request
/// before reading the next line *or* forwarding any event. A slow call
/// therefore froze that client completely - no answers, no messages, no
/// presence - and "slow" is not exotic here. Reading Sneedchat's room list is
/// a Tor round trip through a proof-of-work gate, about fifteen seconds, and
/// for all of it the window showed "Connecting…" and stopped updating.
///
/// So a request becomes a task, and answers come back through a channel that
/// this loop owns. Responses may now arrive out of order; that was always
/// allowed, since every response carries the id of the request it answers and
/// the client matches on it.
async fn handle_connection(state: AppState, stream: crate::ipc::Conn) -> Result<()> {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();
    let subscriptions: Subscriptions = Default::default();
    let mut events = state.events.subscribe();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS));

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break }; // EOF: client disconnected
                if line.is_empty() {
                    continue;
                }
                let state = state.clone();
                let subscriptions = subscriptions.clone();
                let out_tx = out_tx.clone();
                let permits = permits.clone();
                tokio::spawn(async move {
                    // Dropped when this task ends, which is what makes the
                    // limit a queue rather than a refusal.
                    let _permit = permits.acquire().await;
                    // A panic here used to take the whole connection down with
                    // it, since the loop awaited the call directly. Now it
                    // would instead leave one request unanswered forever -
                    // a promise nothing ever settles - so it is caught and
                    // reported as the failure it is.
                    let answered = std::panic::AssertUnwindSafe(handle_line(&state, &line, &subscriptions))
                        .catch_unwind()
                        .await;
                    let response = answered.unwrap_or_else(|_| {
                        tracing::error!("rpc: request handler panicked");
                        wire::err_response(&serde_json::Value::Null, "internal error (see nobilis logs)").to_string()
                    });
                    let _ = out_tx.send(response);
                });
            }
            response = out_rx.recv() => {
                // Only ever None once every sender is gone, and this loop
                // holds one - so this is the compiler's case, not a real one.
                let Some(response) = response else { break };
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
                        Some(id) if subscriptions.lock().unwrap().contains(id) => {}
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

async fn handle_line(state: &AppState, line: &str, subscriptions: &Subscriptions) -> String {
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
