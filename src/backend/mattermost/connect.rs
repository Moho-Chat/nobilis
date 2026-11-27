//! An account's connection: the websocket, kept up and reconnected - resumed where it left off when the server still
//! holds the missed events, loaded afresh when it does not - and stopped, not retried, when the server has ended the
//! session (the same token can never work again).
//!
//! One connection, authenticated in its upgrade's header and again by `authentication_challenge` (the token is never in
//! the address), then `hello`,
//! which names the connection, and the events after it, each numbered (`seq`). A reconnect asks for the same connection
//! and the number after the last one heard; if the server answers `hello` with the same connection id, everything
//! missed is replayed and nothing has to be loaded again. A ping goes every thirty seconds, as Mattermost's own app sends
//! one; a connection that has heard nothing for ninety is let go and made again.

use super::api::{Api, ApiError};
use super::world::{ServerInfo, World};
use super::{events, Handle};
use crate::accounts::MattermostAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message as Ws;

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(5);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(300);
const PING_EVERY: Duration = Duration::from_secs(30);
const SILENT_FOR: Duration = Duration::from_secs(90);
/// How many pings go between asking whether the session still is one (every five minutes).
const SESSION_CHECK_PINGS: u64 = 10;

/// How a connection ended.
#[derive(Debug, PartialEq)]
pub enum Ending {
    /// The server ended the session: sign in again.
    SignInAgain(String),
    /// The connection ended; try again after the usual wait.
    Closed,
}

/// What carries over from one connection to the next: the connection to resume, the next event's number, and what the
/// account could see.
#[derive(Default)]
struct Carry {
    connection_id: String,
    next_seq: i64,
    world: Option<Arc<Mutex<World>>>,
}

/// Starts (or restarts) an account's connection.
pub fn spawn(state: AppState, config: MattermostAccountConfig) {
    let account_id = config.account_id();
    crate::net::route::router().mark(&account_id, config.use_tor);
    state.runtime.reset_connection(&account_id);
    let task = tokio::spawn({
        let account_id = account_id.clone();
        let state = state.clone();
        async move {
            run_with_retry(&state, &account_id).await;
            state.runtime.remove_task_handle(&account_id);
        }
    });
    state.runtime.insert_task_handle(&account_id, task.abort_handle());
}

async fn run_with_retry(state: &AppState, account_id: &str) {
    let mut delay = RECONNECT_INITIAL_DELAY;
    let mut carry = Carry::default();
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    loop {
        // The account as it is now: removed while this slept, it is not reconnected.
        let Some(config) = state.accounts.get_mattermost(account_id) else { return };
        let detail = match run(state, &config, account_id, &mut carry).await {
            Ok(Ending::SignInAgain(why)) => {
                tracing::info!("mattermost[{account_id}]: stopped: {why}");
                state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some(&format!("{why}. Sign in to Mattermost again from Accounts.")));
                return;
            }
            Ok(Ending::Closed) => "connection ended".to_string(),
            Err(e) => match e.downcast_ref::<ApiError>() {
                Some(ApiError::Unauthorized) => {
                    state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some("Mattermost has ended this session. Sign in to Mattermost again from Accounts."));
                    return;
                }
                _ => {
                    tracing::warn!("mattermost[{account_id}]: {e:#}");
                    format!("{e:#}")
                }
            },
        };
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        let wait = state.runtime.next_retry_delay(account_id, &mut delay, RECONNECT_INITIAL_DELAY, RECONNECT_MAX_DELAY);
        tracing::info!("mattermost[{account_id}]: not connected ({detail}); trying again in {}s", wait.as_secs());
        state.runtime.report_retry(state, account_id, &detail, wait);
        tokio::time::sleep(wait).await;
    }
}

/// The websocket's address: the server's own, with `ws` for `http`, and the connection to resume where there is one.
pub fn socket_address(server: &str, connection_id: &str, next_seq: i64) -> Result<String> {
    let mut url = url::Url::parse(&format!("{}/api/v4/websocket", server.trim_end_matches('/'))).context("the server's address")?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme).map_err(|_| anyhow!("the server's address"))?;
    if !connection_id.is_empty() {
        url.query_pairs_mut().append_pair("connection_id", connection_id).append_pair("sequence_number", &next_seq.to_string());
    }
    Ok(url.to_string())
}

/// One go at being connected.
async fn run(state: &AppState, config: &MattermostAccountConfig, account_id: &str, carry: &mut Carry) -> Result<Ending> {
    // A routed account waits for its route: moho's Tor started, or the proxy's address read.
    let router = crate::net::route::router();
    if router.routed(account_id) {
        router.ready(|msg| state.runtime.report_progress(state, account_id, msg)).await.context("starting Tor or reaching the proxy")?;
    }
    let api = Arc::new(Api::new(account_id, &config.server, Some(config.token.clone())));
    // Who the session is, which also finds out whether it still is one.
    let me = api.get("/api/v4/users/me", &[]).await.map_err(anyhow::Error::from)?;
    let own_id = me["id"].as_str().ok_or_else(|| anyhow!("the server did not say who this is"))?.to_string();
    let client_config = api.get("/api/v4/config/client", &[("format", "old".to_string())]).await.map_err(anyhow::Error::from)?;
    let plugins = api.get("/api/v4/plugins/webapp", &[]).await.unwrap_or(Value::Null);
    let license = api.get("/api/v4/license/client", &[("format", "old".to_string())]).await.unwrap_or(Value::Null);
    let server = Arc::new(ServerInfo::from_config(&client_config, &plugins, &license));
    super::media::set_image_proxy(account_id, client_config["HasImageProxy"].as_str() == Some("true"));
    let username = me["username"].as_str().unwrap_or("").to_string();
    if username != config.username {
        let _ = state.accounts.mutate_mattermost(account_id, |a| a.username = username.clone());
    }

    let address = socket_address(&config.server, &carry.connection_id, carry.next_seq)?;
    // The session goes in the upgrade's header, as Mattermost's own apps send theirs: a connection the server knows the
    // session of at once is one it can resume, and one it learns of only from the challenge after is always new.
    let mut request = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(address.as_str()).context("the websocket's address")?;
    request.headers_mut().insert("Authorization", format!("Bearer {}", config.token).parse().context("the session")?);
    let (socket, _) = crate::net::route::websocket(account_id, request).await.context("connecting to the websocket")?;
    let (mut sink, mut stream) = socket.split();
    let (outgoing_tx, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<String>();
    let world = carry.world.clone().unwrap_or_else(|| Arc::new(Mutex::new(World::default())));
    {
        let mut w = world.lock().unwrap_or_else(|e| e.into_inner());
        w.own_id = own_id.clone();
        w.own_session = config.session_id.clone();
    }
    if let Some(user) = super::world::parse_user(&me) {
        let mut w = world.lock().unwrap_or_else(|e| e.into_inner());
        w.system_roles = user.roles.split_whitespace().map(str::to_string).collect();
        w.users.insert(user.id.clone(), user);
    }
    let handle = Handle { account_id: account_id.to_string(), api: api.clone(), world: world.clone(), server: server.clone(), socket: outgoing_tx };
    // The challenge too, which a server that took no notice of the header answers; the token is never in an address.
    let mut seq: i64 = 1;
    sink.send(Ws::text(json!({ "seq": seq, "action": "authentication_challenge", "data": { "token": config.token } }).to_string())).await.context("authenticating")?;

    // A connection asked to be resumed is up as soon as it is open: the server sends it what it missed and no `hello`
    // (and does not answer the challenge on a connection its header already signed in). If the server could not resume
    // it, a `hello` with a new connection id comes first, and everything is read again.
    if !carry.connection_id.is_empty() && carry.world.is_some() {
        state.runtime.set_mattermost_handle(account_id, handle.clone());
        state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
        tracing::info!("mattermost[{account_id}]: connected as {own_id} (resuming)");
    }
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut heard = Instant::now();
    let mut pings: u64 = 0;
    let ending = loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { break Ending::Closed };
                let frame = frame.context("reading the websocket")?;
                heard = Instant::now();
                let text = match frame {
                    Ws::Text(t) => t.to_string(),
                    Ws::Close(_) => break Ending::Closed,
                    _ => continue,
                };
                let Ok(event) = serde_json::from_str::<Value>(&text) else { continue };
                // A reply to something this client sent: the authentication's refusal ends the session.
                if event.get("seq_reply").is_some() {
                    if event["seq_reply"].as_i64() == Some(1) && event["status"].as_str() == Some("FAIL") {
                        break Ending::SignInAgain("Mattermost did not accept this session".to_string());
                    }
                    continue;
                }
                if let Some(n) = event["seq"].as_i64() {
                    carry.next_seq = n + 1;
                }
                match event["event"].as_str() {
                    Some("hello") => {
                        let connection = event["data"]["connection_id"].as_str().unwrap_or("").to_string();
                        let resumed = !carry.connection_id.is_empty() && connection == carry.connection_id && carry.world.is_some();
                        carry.connection_id = connection;
                        if !resumed {
                            // A new connection: what was missed is not to be had, so everything is read again.
                            carry.next_seq = 1;
                            events::load(state, &handle).await;
                        }
                        carry.world = Some(world.clone());
                        state.runtime.set_mattermost_handle(account_id, handle.clone());
                        state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
                        tracing::info!("mattermost[{account_id}]: connected as {own_id}{}", if resumed { " (resumed)" } else { "" });
                    }
                    Some(_) if events::handle(state, &handle, &event) == events::Outcome::SignedOut => {
                        break Ending::SignInAgain("Mattermost has ended this session".to_string());
                    }
                    _ => {}
                }
            }
            line = outgoing.recv() => {
                match line {
                    Some(line) => {
                        // Every action is numbered by this connection.
                        let mut v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                        seq += 1;
                        v["seq"] = json!(seq);
                        sink.send(Ws::text(v.to_string())).await.context("writing to the websocket")?;
                    }
                    None => break Ending::Closed,
                }
            }
            _ = ping.tick() => {
                if heard.elapsed() > SILENT_FOR {
                    break Ending::Closed;
                }
                // The server keeps a revoked session's websocket open, so the session itself is asked after now and then;
                // and a request anywhere that was refused for it has already said so.
                pings += 1;
                if api.halted() || (pings.is_multiple_of(SESSION_CHECK_PINGS) && matches!(api.get("/api/v4/users/me", &[]).await, Err(ApiError::Unauthorized))) {
                    break Ending::SignInAgain("Mattermost has ended this session".to_string());
                }
                seq += 1;
                sink.send(Ws::text(json!({ "seq": seq, "action": "ping" }).to_string())).await.context("pinging")?;
            }
        }
    };
    state.runtime.clear_mattermost_handle(account_id);
    let _ = sink.close().await;
    // A session found ended is checked once more by asking who it is: a websocket that closed is not proof.
    if ending == Ending::Closed && api.get("/api/v4/users/me", &[]).await.is_err() && api.halted() {
        return Ok(Ending::SignInAgain("Mattermost has ended this session".to_string()));
    }
    Ok(ending)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_socket_is_the_servers_own_and_carries_no_token() {
        assert_eq!(socket_address("https://chat.example.org", "", 0).unwrap(), "wss://chat.example.org/api/v4/websocket");
        assert_eq!(socket_address("http://127.0.0.1:8065/", "", 0).unwrap(), "ws://127.0.0.1:8065/api/v4/websocket");
        assert_eq!(socket_address("https://example.org/chat", "", 0).unwrap(), "wss://example.org/chat/api/v4/websocket");
        let resumed = socket_address("https://chat.example.org", "abc", 42).unwrap();
        assert_eq!(resumed, "wss://chat.example.org/api/v4/websocket?connection_id=abc&sequence_number=42");
        assert!(!resumed.contains("token"));
    }
}
