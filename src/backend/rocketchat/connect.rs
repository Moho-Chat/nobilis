//! Persistent Meteor DDP WebSocket connection manager for Rocket.Chat.
//!
//! Lifecycle:
//! 1. Discover server capabilities (`/api/info`, `/api/v1/settings.public`).
//! 2. Verify credentials (`GET /api/v1/me`) and fetch initial subscriptions, rooms, teams, custom emoji.
//! 3. Open DDP WebSocket (`wss://<host>/websocket`) through `crate::net::route::websocket`.
//! 4. Handshake `connect` -> `connected`, authenticate via `login` with `{"resume": authToken}`.
//! 5. Subscribe to `stream-room-messages` (`__my_messages__`), `stream-notify-user`
//!    (`rooms-changed`, `subscriptions-changed`, `notification`, `otr`, `webrtc`, `e2ekeyRequest`,
//!    `message`, `video-conference`), `stream-notify-logged` (`user-status`, `Users:NameChanged`,
//!    `updateEmojiCustom`, `roles-change`), and per-room `stream-notify-room` (`typing`, `deleteMessage`,
//!    `deleteMessageBulk`).
//! 6. Keepalive via DDP `ping`/`pong`, exponential backoff on transient disconnect, clean stop on 401/403.

use super::api::{Api, ApiError};
use super::ddp::{self, DdpMessage};
use super::discovery;
use super::e2ee::E2eeState;
use super::world::World;
use super::{events, Handle};
use crate::accounts::RocketChatAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message as Ws;

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(5);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(300);
const PING_INTERVAL: Duration = Duration::from_secs(25);

#[derive(Debug, PartialEq)]
enum Ending {
    AuthFailed(String),
    Closed,
}

/// Spawn the persistent Rocket.Chat connection task for an account.
pub fn spawn(state: AppState, config: RocketChatAccountConfig) {
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
        let Some(config) = state.accounts.get_rocketchat(account_id) else {
            return;
        };
        let started = Instant::now();
        let detail = match run_once(state, &config, account_id).await {
            Ok(Ending::AuthFailed(why)) => {
                tracing::info!("rocketchat[{account_id}]: stopped: {why}");
                state.runtime.clear_rocketchat_handle(account_id);
                state.runtime.set_conn_state(
                    state,
                    account_id,
                    ConnState::AuthFailed,
                    Some(&format!("{why}. Sign in to Rocket.Chat again from Accounts.")),
                );
                return;
            }
            Ok(Ending::Closed) => "connection closed".to_string(),
            Err(e) => match e.downcast_ref::<ApiError>() {
                Some(ApiError::Unauthorized) => {
                    state.runtime.clear_rocketchat_handle(account_id);
                    state.runtime.set_conn_state(
                        state,
                        account_id,
                        ConnState::AuthFailed,
                        Some("Rocket.Chat token expired or revoked. Sign in again from Accounts."),
                    );
                    return;
                }
                _ => {
                    tracing::warn!("rocketchat[{account_id}]: {e:#}");
                    format!("{e:#}")
                }
            },
        };

        state.runtime.clear_rocketchat_handle(account_id);
        if started.elapsed() > Duration::from_secs(120) {
            delay = RECONNECT_INITIAL_DELAY;
        }
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        let wait = state.runtime.next_retry_delay(
            account_id,
            &mut delay,
            RECONNECT_INITIAL_DELAY,
            RECONNECT_MAX_DELAY,
        );
        tracing::info!(
            "rocketchat[{account_id}]: disconnected ({detail}); reconnecting in {}s",
            wait.as_secs()
        );
        state.runtime.report_retry(state, account_id, &detail, wait);
        tokio::time::sleep(wait).await;
    }
}

async fn run_once(
    state: &AppState,
    config: &RocketChatAccountConfig,
    account_id: &str,
) -> Result<Ending> {
    let router = crate::net::route::router();
    if router.routed(account_id) {
        router
            .ready(|msg| state.runtime.report_progress(state, account_id, msg))
            .await
            .context("starting Tor or reaching SOCKS5 proxy")?;
    }

    let server = Arc::new(discovery::discover(account_id, &config.server_url).await?);
    let api = Arc::new(Api::new(
        account_id,
        &server.server_url,
        Some(config.user_id.clone()),
        Some(config.auth_token.clone()),
    ));

    // Verify session & get own identity
    let me = api.get("/api/v1/me", &[]).await.map_err(anyhow::Error::from)?;
    let own_id = me["_id"]
        .as_str()
        .ok_or_else(|| anyhow!("Rocket.Chat /api/v1/me missing _id"))?
        .to_string();
    let own_username = me["username"].as_str().unwrap_or(&config.username).to_string();
    let own_name = me["name"].as_str().unwrap_or(&own_username).to_string();
    let own_roles: Vec<String> = me["roles"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    let mut world_inner = World {
        own_id: own_id.clone(),
        own_username: own_username.clone(),
        own_name,
        own_roles,
        server_host: config.server_host.clone(),
        ..Default::default()
    };

    // Fetch initial subscriptions and rooms
    if let Ok(subs) = api.get("/api/v1/subscriptions.get", &[]).await {
        for s in subs["update"].as_array().into_iter().flatten() {
            world_inner.merge_subscription_json(s);
        }
    }
    if let Ok(rooms) = api.get("/api/v1/rooms.get", &[]).await {
        for r in rooms["update"].as_array().into_iter().flatten() {
            world_inner.merge_room_json(r);
        }
    }

    // Fetch Omnichannel / Livechat rooms if enabled on account and user holds agent role
    if config.omnichannel_enabled && world_inner.is_livechat_agent() {
        if let Ok(lc) = api.get("/api/v1/livechat/rooms", &[("open", "true".into())]).await {
            for r in lc["rooms"].as_array().into_iter().flatten() {
                world_inner.merge_room_json(r);
            }
        }
    }

    let ws_url = super::websocket_url(&server.server_url)?;
    let (socket, _) = crate::net::route::websocket(account_id, ws_url)
        .await
        .context("connecting to Rocket.Chat DDP WebSocket")?;
    let (mut sink, mut stream) = socket.split();
    let (ddp_tx, mut ddp_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let world = Arc::new(Mutex::new(world_inner));
    let e2ee = Arc::new(Mutex::new(E2eeState::default()));
    let handle = Handle {
        account_id: account_id.to_string(),
        api: api.clone(),
        server: server.clone(),
        world: world.clone(),
        ddp: ddp_tx.clone(),
        e2ee: e2ee.clone(),
    };

    // Start DDP handshake
    sink.send(Ws::text(ddp::connect_frame()))
        .await
        .context("sending DDP connect")?;

    let login_req_id = ddp::next_id("login");
    let mut logged_in = false;
    let mut ping_timer = tokio::time::interval(PING_INTERVAL);
    ping_timer.reset();

    let ending = loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { break Ending::Closed };
                let text = match frame.context("reading DDP WebSocket")? {
                    Ws::Text(t) => t.to_string(),
                    Ws::Close(_) => break Ending::Closed,
                    Ws::Ping(payload) => {
                        let _ = sink.send(Ws::Pong(payload)).await;
                        continue;
                    }
                    _ => continue,
                };
                let Some(ddp_msg) = ddp::parse_frame(&text) else { continue };
                match ddp_msg {
                    DdpMessage::Connected { .. } => {
                        let frame = ddp::login_resume_frame(&login_req_id, &config.auth_token);
                        sink.send(Ws::text(frame)).await.context("sending DDP login")?;
                    }
                    DdpMessage::Ping { id } => {
                        sink.send(Ws::text(ddp::pong_frame(id.as_deref()))).await.context("sending DDP pong")?;
                    }
                    DdpMessage::Result { id, error, .. } if id == login_req_id => {
                        if let Some(err) = error {
                            if err.is_auth_failure() {
                                break Ending::AuthFailed(format!("DDP login rejected: {}", err.reason));
                            }
                            tracing::warn!("rocketchat[{account_id}]: DDP login error: {}", err.reason);
                            break Ending::Closed;
                        }
                        logged_in = true;
                        subscribe_all(&handle, &own_id);
                        events::register_server_rail(state, &handle);
                        let snapshot: Vec<_> = handle
                            .world
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .rooms
                            .values()
                            .cloned()
                            .collect();
                        for room in &snapshot {
                            events::register_room(state, &handle, room);
                            subscribe_room(&handle, &room.id);
                        }
                        state.runtime.set_rocketchat_handle(account_id, handle.clone());
                        state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
                        tracing::info!("rocketchat[{account_id}]: connected as @{own_username} ({} rooms)", snapshot.len());

                        // Background sync: custom emoji + E2EE keys unlock if passphrase stored + default status
                        let bg_state = state.clone();
                        let bg_handle = handle.clone();
                        let passphrase = config.e2ee_passphrase.clone();
                        let default_status = config.default_status.clone();
                        tokio::spawn(async move {
                            let _ = super::media::sync_custom_emoji(&bg_state, &bg_handle).await;
                            if bg_handle.server.e2ee_enabled {
                                if let Some(pass) = passphrase.filter(|p| !p.is_empty()) {
                                    if let Err(e) = super::e2ee::unlock_with_passphrase(&bg_handle, &pass).await {
                                        tracing::warn!("rocketchat[{}]: E2EE auto-unlock failed: {e}", bg_handle.account_id);
                                    }
                                }
                            }
                            if let Some(st) = default_status.filter(|s| !s.is_empty()) {
                                let _ = super::profile::set_status(&bg_handle, &st, None).await;
                            }
                        });
                    }
                    DdpMessage::Changed { .. } if logged_in => {
                        events::handle_ddp(state, &handle, ddp_msg);
                    }
                    DdpMessage::Error { reason } => {
                        tracing::warn!("rocketchat[{account_id}]: DDP error: {reason}");
                    }
                    _ => {}
                }
            }
            Some(out) = ddp_rx.recv() => {
                if sink.send(Ws::text(out)).await.is_err() {
                    break Ending::Closed;
                }
            }
            _ = ping_timer.tick() => {
                let pid = ddp::next_id("ping");
                if sink.send(Ws::text(ddp::ping_frame(Some(&pid)))).await.is_err() {
                    break Ending::Closed;
                }
            }
        }
    };

    Ok(ending)
}

fn subscribe_all(handle: &Handle, own_id: &str) {
    let subs = vec![
        ("stream-room-messages", json!(["__my_messages__", false])),
        ("stream-notify-user", json!([format!("{own_id}/rooms-changed"), false])),
        ("stream-notify-user", json!([format!("{own_id}/subscriptions-changed"), false])),
        ("stream-notify-user", json!([format!("{own_id}/notification"), false])),
        ("stream-notify-user", json!([format!("{own_id}/otr"), false])),
        ("stream-notify-user", json!([format!("{own_id}/webrtc"), false])),
        ("stream-notify-user", json!([format!("{own_id}/e2ekeyRequest"), false])),
        ("stream-notify-user", json!([format!("{own_id}/message"), false])),
        ("stream-notify-user", json!([format!("{own_id}/video-conference"), false])),
        ("stream-notify-logged", json!(["user-status", false])),
        ("stream-notify-logged", json!(["Users:NameChanged", false])),
        ("stream-notify-logged", json!(["updateEmojiCustom", false])),
        ("stream-notify-logged", json!(["roles-change", false])),
    ];
    for (name, params) in subs {
        let id = ddp::next_id("sub");
        let _ = handle.ddp.send(ddp::sub_frame(&id, name, params));
    }
}

pub fn subscribe_room(handle: &Handle, room_id: &str) {
    {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if !world.subscribed_rooms.insert(room_id.to_string()) {
            return;
        }
    }
    for suffix in ["typing", "user-activity", "deleteMessage", "deleteMessageBulk"] {
        let id = ddp::next_id("rsub");
        let _ = handle.ddp.send(ddp::sub_frame(
            &id,
            "stream-notify-room",
            json!([format!("{room_id}/{suffix}"), false]),
        ));
    }
}
