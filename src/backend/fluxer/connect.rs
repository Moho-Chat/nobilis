//! An account's connection: the gateway, kept up and resumed - without ever signing in afresh to get over a hiccup,
//! and stopping, not retrying, when Fluxer has ended the session.
//!
//! The gateway is Discord's in shape (`gateway/overview.md`): Hello with a heartbeat interval; Identify with the token
//! (in a message, never in the address); `READY`; Dispatches with a sequence; a heartbeat carrying the last sequence
//! handled, and answering the server's own heartbeat at once. A dropped connection is resumed within the minute Fluxer
//! keeps a session for, with the same session id and the last sequence; Reconnect (op 7) asks for exactly that, and
//! Invalid Session (op 9) means a new Identify. Close 4004 is a token that no longer works, which ends the account until
//! the person signs in again; 4010-4012 are moho's own mistakes and are not retried in a loop either.
//!
//! A user session is "passive" in a guild of more than 250 members until a Lazy Request makes it active
//! (`gateway/event-filtering.md`), and hears only mentions and its own messages there. A guild is made active when one
//! of its conversations is opened, and stays so for the connection.

use super::api::{Api, ApiError};
use super::discovery;
use super::world::World;
use super::{events, Handle};
use crate::accounts::FluxerAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as Ws;

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(5);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(300);
/// How long Fluxer keeps a dropped session to be resumed.
const RESUMABLE_FOR: Duration = Duration::from_secs(55);

/// How a connection ended.
#[derive(Debug, PartialEq)]
pub enum Ending {
    /// Fluxer ended the session, or will never accept this one: sign in again.
    SignInAgain(String),
    /// Something moho sent was wrong; trying again in a loop would only repeat it.
    Stop(String),
    /// The connection ended; try again (resuming if it can be) after the usual wait.
    Closed,
    /// Fluxer asked for a new connection at once, resumed.
    Reconnect,
}

/// What a close code means for the connection (`gateway/opcodes-and-close-codes.md`).
pub fn classify(code: u16) -> Ending {
    match code {
        4004 => Ending::SignInAgain("Fluxer has ended this session".to_string()),
        4010..=4012 => Ending::Stop(format!("Fluxer refused moho's connection ({code})")),
        _ => Ending::Closed,
    }
}

/// A session that may be resumed: its id, the last sequence handled, and when the connection dropped.
#[derive(Default, Clone)]
struct Resumable {
    session_id: String,
    seq: Option<u64>,
    dropped: Option<Instant>,
}

pub fn spawn(state: AppState, config: FluxerAccountConfig) {
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
    let mut resume = Resumable::default();
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    loop {
        let Some(config) = state.accounts.get_fluxer(account_id) else { return };
        let started = Instant::now();
        let detail = match run(state, &config, account_id, &mut resume).await {
            Ok(Ending::SignInAgain(why)) => {
                tracing::info!("fluxer[{account_id}]: stopped: {why}");
                state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some(&format!("{why}. Sign in to Fluxer again from Accounts.")));
                return;
            }
            Ok(Ending::Stop(why)) => {
                tracing::warn!("fluxer[{account_id}]: stopped: {why}");
                state.runtime.set_conn_state(state, account_id, ConnState::Disconnected, Some(&why));
                return;
            }
            Ok(Ending::Reconnect) => {
                state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
                continue;
            }
            Ok(Ending::Closed) => "connection ended".to_string(),
            Err(e) => match e.downcast_ref::<ApiError>() {
                Some(ApiError::Unauthorized) => {
                    state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some("Fluxer has ended this session. Sign in to Fluxer again from Accounts."));
                    return;
                }
                _ => {
                    tracing::warn!("fluxer[{account_id}]: {e:#}");
                    format!("{e:#}")
                }
            },
        };
        // A connection that held a while starts the wait afresh.
        if started.elapsed() > Duration::from_secs(120) {
            delay = RECONNECT_INITIAL_DELAY;
        }
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        let wait = state.runtime.next_retry_delay(account_id, &mut delay, RECONNECT_INITIAL_DELAY, RECONNECT_MAX_DELAY);
        tracing::info!("fluxer[{account_id}]: not connected ({detail}); trying again in {}s", wait.as_secs());
        state.runtime.report_retry(state, account_id, &detail, wait);
        tokio::time::sleep(wait).await;
    }
}

/// The gateway's address, as the instance gave it, with the version and encoding added to whatever query it has.
pub fn gateway_address(gateway: &str) -> Result<String> {
    let mut url = url::Url::parse(gateway).context("the instance's gateway address")?;
    if url.path().is_empty() {
        url.set_path("/");
    }
    url.query_pairs_mut().append_pair("v", "1").append_pair("encoding", "json");
    Ok(url.to_string())
}

/// What moho says of itself on Identify: the real system, and its own name.
pub fn properties() -> Value {
    let os = match std::env::consts::OS {
        "linux" => "Linux",
        "windows" => "Windows",
        "macos" => "Mac OS X",
        other => other,
    };
    json!({ "os": os, "browser": "moho", "device": "desktop" })
}

async fn run(state: &AppState, config: &FluxerAccountConfig, account_id: &str, resume: &mut Resumable) -> Result<Ending> {
    let origin = if config.instance.is_empty() { discovery::DEFAULT_ORIGIN } else { config.instance.as_str() };
    let instance = Arc::new(discovery::fetch(account_id, origin).await?);
    let api = Arc::new(Api::new(account_id, &instance.api, Some(config.token.clone())));
    // Who the session is, which also finds out whether it still is one.
    let me = api.get("/users/@me", &[]).await.map_err(anyhow::Error::from)?;
    let own_id = me["id"].as_str().ok_or_else(|| anyhow!("Fluxer did not say who this is"))?.to_string();
    let username = super::login::username_of(&me, &instance);
    if username != config.username {
        let _ = state.accounts.mutate_fluxer(account_id, |a| a.username = username);
    }
    let premium = me["premium_type"].as_i64().unwrap_or(0) > 0;

    let (socket, _) = crate::net::route::websocket(account_id, gateway_address(&instance.gateway)?).await.context("connecting to the gateway")?;
    let (mut sink, mut stream) = socket.split();
    let (gateway, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<String>();
    let world = Arc::new(Mutex::new(World { own_id: own_id.clone(), ..Default::default() }));
    if let Some((id, user)) = super::world::parse_user(&me) {
        world.lock().unwrap_or_else(|e| e.into_inner()).users.insert(id, user);
    }
    let limits = instance.limits(if premium { &["premium"] } else { &[] });
    let handle = Handle { account_id: account_id.to_string(), api: api.clone(), world: world.clone(), instance: instance.clone(), gateway, active: Arc::new(Mutex::new(HashSet::new())), limits: Arc::new(limits), slow: Arc::default() };

    let can_resume = !resume.session_id.is_empty() && resume.dropped.is_some_and(|d| d.elapsed() < RESUMABLE_FOR);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(3600));
    heartbeat.reset();
    let mut acked = true;
    let mut ready = false;
    let ending = loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { break Ending::Closed };
                let text = match frame.context("reading the gateway")? {
                    Ws::Text(t) => t.to_string(),
                    Ws::Close(close) => break close.map(|c| { tracing::info!("fluxer[{account_id}]: gateway closed {} {}", u16::from(c.code), c.reason); classify(c.code.into()) }).unwrap_or(Ending::Closed),
                    _ => continue,
                };
                let Ok(payload) = serde_json::from_str::<Value>(&text) else { continue };
                match payload["op"].as_i64() {
                    Some(10) => {
                        let every = payload["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250).clamp(5_000, 120_000);
                        heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_millis(every / 2), Duration::from_millis(every));
                        let hello = if can_resume {
                            tracing::info!("fluxer[{account_id}]: resuming");
                            json!({ "op": 6, "d": { "token": config.token, "session_id": resume.session_id, "seq": resume.seq.unwrap_or(0) } })
                        } else {
                            resume.seq = None;
                            identify(&config.token)
                        };
                        sink.send(Ws::text(hello.to_string())).await.context("identifying")?;
                    }
                    Some(11) => acked = true,
                    Some(1) => sink.send(Ws::text(json!({ "op": 1, "d": resume.seq }).to_string())).await.context("answering a heartbeat")?,
                    Some(7) => break Ending::Reconnect,
                    Some(9) => {
                        // The session cannot be resumed: a new one on this same socket.
                        tracing::info!("fluxer[{account_id}]: session not resumable; identifying afresh");
                        resume.session_id.clear();
                        resume.seq = None;
                        sink.send(Ws::text(identify(&config.token).to_string())).await.context("identifying")?;
                    }
                    Some(12) => events::gateway_error(state, &handle, &payload["d"]),
                    Some(0) => {
                        if let Some(s) = payload["s"].as_u64() {
                            resume.seq = Some(resume.seq.map_or(s, |old| old.max(s)));
                        }
                        let kind = payload["t"].as_str().unwrap_or("");
                        let data = &payload["d"];
                        match kind {
                            "READY" => {
                                resume.session_id = data["session_id"].as_str().unwrap_or("").to_string();
                                events::ready(state, &handle, data);
                                state.runtime.set_fluxer_handle(account_id, handle.clone());
                                state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
                                ready = true;
                                tracing::info!("fluxer[{account_id}]: connected as {own_id}");
                            }
                            "RESUMED" => {
                                if !ready {
                                    // A resumed session has no READY: what the account can see is read again.
                                    events::refetch(state, &handle).await;
                                    state.runtime.set_fluxer_handle(account_id, handle.clone());
                                    state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
                                    ready = true;
                                }
                                tracing::info!("fluxer[{account_id}]: resumed");
                            }
                            _ => events::handle(state, &handle, kind, data),
                        }
                    }
                    _ => {}
                }
            }
            line = outgoing.recv() => {
                match line {
                    Some(line) => sink.send(Ws::text(line)).await.context("writing to the gateway")?,
                    None => break Ending::Closed,
                }
            }
            _ = heartbeat.tick() => {
                if !acked {
                    tracing::info!("fluxer[{account_id}]: no heartbeat acknowledgement; reconnecting");
                    break Ending::Closed;
                }
                acked = false;
                sink.send(Ws::text(json!({ "op": 1, "d": resume.seq }).to_string())).await.context("heartbeating")?;
            }
        }
    };
    state.runtime.clear_fluxer_handle(account_id);
    resume.dropped = Some(Instant::now());
    if matches!(ending, Ending::SignInAgain(_) | Ending::Stop(_)) {
        resume.session_id.clear();
    }
    // Not 1000 or 1001, which would end the session at Fluxer: 4000 leaves it there to resume.
    let _ = sink.send(Ws::Close(Some(CloseFrame { code: CloseCode::from(4000), reason: "".into() }))).await;
    let _ = sink.close().await;
    Ok(ending)
}

fn identify(token: &str) -> Value {
    json!({ "op": 2, "d": { "token": token, "properties": properties() } })
}

/// Makes the guild a channel is in active for the session, so its messages and typing are heard (see the module's
/// words on passive guilds). Once per guild per connection.
pub fn activate_for(handle: &Handle, channel_id: &str) {
    let Some(guild) = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(channel_id).and_then(|c| c.guild.clone()) else { return };
    if !handle.active.lock().unwrap_or_else(|e| e.into_inner()).insert(guild.clone()) {
        return;
    }
    let subscriptions: serde_json::Map<String, Value> = handle.active.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|g| (g.clone(), json!({ "active": true, "typing": true }))).collect();
    let _ = handle.gateway.send(json!({ "op": 14, "d": { "subscriptions": subscriptions } }).to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revoked_token_stops_the_account_and_most_closes_are_tried_again() {
        assert!(matches!(classify(4004), Ending::SignInAgain(_)));
        assert!(matches!(classify(4012), Ending::Stop(_)));
        assert_eq!(classify(4000), Ending::Closed);
        assert_eq!(classify(4008), Ending::Closed);
        assert_eq!(classify(1006), Ending::Closed);
    }

    #[test]
    fn the_gateway_address_keeps_its_own_query_and_has_no_token() {
        assert_eq!(gateway_address("wss://gateway.fluxer.com").unwrap(), "wss://gateway.fluxer.com/?v=1&encoding=json");
        assert_eq!(gateway_address("ws://localhost:8890/gateway").unwrap(), "ws://localhost:8890/gateway?v=1&encoding=json");
        assert_eq!(gateway_address("wss://g.example/?region=eu").unwrap(), "wss://g.example/?region=eu&v=1&encoding=json");
        assert!(gateway_address("not a url").is_err());
    }

    #[test]
    fn identify_names_moho_honestly() {
        let p = properties();
        assert_eq!(p["browser"], "moho");
        assert!(p["os"].as_str().is_some_and(|o| !o.is_empty()));
    }
}
