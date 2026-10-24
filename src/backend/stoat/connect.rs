//! An account's connection: the events websocket, kept up and reconnected - without ever signing in afresh to get over
//! a hiccup, and stopping, not retrying, when Stoat has ended the session (the same token can never work again).
//!
//! The shape is Telegram's and Steam's: a wait that grows while a connection falls over at once and starts again after
//! one that held, a long wait when told to slow down, and a stop when the session is over.
//!
//! One connection, one `Authenticate` (the token goes in a message, never in the address), then `Ready`, which carries
//! everything the account can see, and the events after it. A `Ping` goes every twenty seconds, which Stoat asks of a
//! client (every ten to thirty); a connection that has heard nothing for ninety is let go and made again.

use super::api::{self, Api, ApiError};
use super::world::{Instance, World};
use super::{events, Handle};
use crate::accounts::StoatAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message as Ws;

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(10);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(300);
const PING_EVERY: Duration = Duration::from_secs(20);
const SILENT_FOR: Duration = Duration::from_secs(90);

/// How a connection ended.
#[derive(Debug, PartialEq)]
pub enum Ending {
    /// Stoat ended the session: sign in again.
    SignInAgain(String),
    /// The connection ended; try again after the usual wait.
    Closed,
}

/// What Stoat's `Error` event means for the connection.
pub fn classify(error: &str) -> Ending {
    match error {
        "InvalidSession" | "NotAuthenticated" => Ending::SignInAgain("Stoat has ended this session".to_string()),
        "OnboardingNotFinished" => Ending::SignInAgain("This account has not finished setting up: choose a username in Stoat's own app first".to_string()),
        _ => Ending::Closed,
    }
}

/// Starts (or restarts) an account's connection.
pub fn spawn(state: AppState, config: StoatAccountConfig) {
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
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    loop {
        // The account as it is now: removed while this slept, it is not reconnected.
        let Some(config) = state.accounts.get_stoat(account_id) else { return };
        let detail = match run(state, &config, account_id).await {
            Ok(Ending::SignInAgain(why)) => {
                tracing::info!("stoat[{account_id}]: stopped: {why}");
                state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some(&format!("{why}. Sign in to Stoat again from Accounts.")));
                return;
            }
            Ok(Ending::Closed) => "connection ended".to_string(),
            Err(e) => match e.downcast_ref::<ApiError>() {
                Some(ApiError::Unauthorized) => {
                    state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some("Stoat has ended this session. Sign in to Stoat again from Accounts."));
                    return;
                }
                _ => {
                    tracing::warn!("stoat[{account_id}]: {e:#}");
                    format!("{e:#}")
                }
            },
        };
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        let wait = state.runtime.next_retry_delay(account_id, &mut delay, RECONNECT_INITIAL_DELAY, RECONNECT_MAX_DELAY);
        tracing::info!("stoat[{account_id}]: not connected ({detail}); trying again in {}s", wait.as_secs());
        state.runtime.report_retry(state, account_id, &detail, wait);
        tokio::time::sleep(wait).await;
    }
}

/// The address of the events socket: the instance's own, asking for what is wanted of `Ready` and nothing else. The
/// path is the instance's own and no slash is added to it: a self-hosted instance serves its events at `/ws` exactly,
/// and `/ws/` is somewhere else. An address with no path at all (the official one's) is written with the `/` a request
/// line needs.
fn socket_address(instance: &Instance) -> Result<String> {
    let mut url = url::Url::parse(instance.ws.trim_end_matches('/')).context("the instance's events address")?;
    url.set_query(Some("version=1&format=json&ready=users&ready=servers&ready=channels&ready=members&ready=emojis&ready=voice_states&ready=channel_unreads"));
    Ok(url.to_string())
}

/// One go at being connected.
async fn run(state: &AppState, config: &StoatAccountConfig, account_id: &str) -> Result<Ending> {
    let instance_url = if config.instance.is_empty() { api::DEFAULT_INSTANCE } else { config.instance.as_str() };
    let api = Arc::new(Api::new(account_id, instance_url, Some(config.token.clone())));
    let root = api::instance_info(&api).await?;
    let instance = Arc::new(Instance::from_root(&root));
    if instance.ws.is_empty() {
        return Err(anyhow!("the instance did not say where its events are"));
    }
    // Who the session is, which also finds out whether it still is one.
    let me = api.get("/users/@me", &[]).await.map_err(anyhow::Error::from)?;
    let own_id = me["_id"].as_str().ok_or_else(|| anyhow!("Stoat did not say who this is"))?.to_string();
    let username = format!("{}#{}", me["username"].as_str().unwrap_or(""), me["discriminator"].as_str().unwrap_or(""));
    let _ = state.accounts.mutate_stoat(account_id, |a| a.username = username);

    let (mut sink, mut stream) = route_socket(account_id, &instance).await?.split();
    let (socket, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<String>();
    let world = Arc::new(Mutex::new(World::default()));
    let handle = Handle { account_id: account_id.to_string(), api: api.clone(), world: world.clone(), instance: instance.clone(), socket };
    // The token goes in a message, so it is never in an address a log could hold.
    sink.send(Ws::text(json!({ "type": "Authenticate", "token": config.token }).to_string())).await.context("authenticating")?;

    let mut ping = tokio::time::interval(PING_EVERY);
    let mut pings: u64 = 0;
    let mut heard = Instant::now();
    let ending = loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { break Ending::Closed };
                let frame = frame.context("reading the events")?;
                heard = Instant::now();
                let text = match frame {
                    Ws::Text(t) => t.to_string(),
                    Ws::Close(_) => break Ending::Closed,
                    _ => continue,
                };
                let Ok(event) = serde_json::from_str::<Value>(&text) else { continue };
                match event["type"].as_str() {
                    Some("Authenticated") | Some("Pong") => {}
                    Some("Ready") => {
                        // The account is in the world even when Stoat names only those it has a relation with.
                        if let Some((id, user)) = super::world::parse_user(&me) {
                            world.lock().unwrap_or_else(|e| e.into_inner()).users.insert(id, user);
                        }
                        events::ready(state, &handle, &own_id, &event);
                        state.runtime.set_stoat_handle(account_id, handle.clone());
                        state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
                        tracing::info!("stoat[{account_id}]: connected as {own_id}");
                        // What the account has muted lives in its settings, which are asked for once it is up.
                        let (state, handle) = (state.clone(), handle.clone());
                        tokio::spawn(async move { super::settings::load(&state, &handle).await });
                    }
                    Some("Error") => {
                        let error = event["error"].as_str().unwrap_or("");
                        tracing::info!("stoat[{account_id}]: the events said {error}");
                        match classify(error) {
                            Ending::Closed => {}
                            other => break other,
                        }
                    }
                    Some("Logout") => break Ending::SignInAgain("Stoat has ended this session".to_string()),
                    Some("Auth") => {
                        // A session deleted: this one, or one of the account's others.
                        let ours = event["session_id"].as_str().is_some_and(|s| s == config.session_id) || event["event_type"].as_str() == Some("DeleteAllSessions");
                        if ours && !config.session_id.is_empty() {
                            break Ending::SignInAgain("Stoat has ended this session".to_string());
                        }
                    }
                    _ => events::handle(state, &handle, &event),
                }
            }
            line = outgoing.recv() => {
                match line {
                    Some(line) => sink.send(Ws::text(line)).await.context("writing to the events")?,
                    None => break Ending::Closed,
                }
            }
            _ = ping.tick() => {
                if heard.elapsed() > SILENT_FOR {
                    break Ending::Closed;
                }
                pings += 1;
                sink.send(Ws::text(json!({ "type": "Ping", "data": pings }).to_string())).await.context("pinging")?;
            }
        }
    };
    state.runtime.clear_stoat_handle(account_id);
    let _ = sink.close().await;
    Ok(ending)
}

async fn route_socket(account_id: &str, instance: &Instance) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<crate::net::route::TcpOrSocks>>> {
    let (socket, _) = crate::net::route::websocket(account_id, socket_address(instance)?).await.context("connecting to the events")?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_stoat_ended_stops_the_connection_and_anything_else_is_tried_again() {
        assert!(matches!(classify("InvalidSession"), Ending::SignInAgain(_)));
        assert!(matches!(classify("NotAuthenticated"), Ending::SignInAgain(_)));
        assert!(matches!(classify("OnboardingNotFinished"), Ending::SignInAgain(w) if w.contains("username")));
        assert_eq!(classify("InternalError"), Ending::Closed);
        assert_eq!(classify("AlreadyAuthenticated"), Ending::Closed);
    }

    #[test]
    fn the_socket_asks_for_what_ready_is_needed_for_and_has_no_token_in_it() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let path_and_query = |ws: &str| -> String {
            let address = socket_address(&Instance { ws: ws.into(), ..Default::default() }).unwrap();
            // As the websocket library writes it in the request line.
            address.as_str().into_client_request().unwrap().uri().path_and_query().unwrap().to_string()
        };
        // The official instance has no path, and a request line needs one.
        let official = path_and_query("wss://events.example/");
        assert!(official.starts_with("/?version=1&format=json"), "{official}");
        assert!(path_and_query("wss://events.example").starts_with("/?version=1"));
        // A self-hosted one serves its events at /ws, and /ws/ is somewhere else.
        let hosted = path_and_query("ws://localhost:8880/ws");
        assert!(hosted.starts_with("/ws?version=1&format=json"), "{hosted}");
        for want in ["ready=users", "ready=servers", "ready=channels", "ready=members", "ready=emojis", "ready=voice_states", "ready=channel_unreads"] {
            assert!(official.contains(want), "{official} lacks {want}");
        }
        assert!(!official.contains("token"), "the token goes in a message");
        assert!(socket_address(&Instance::default()).is_err(), "an instance that did not say where its events are has no address");
    }
}
