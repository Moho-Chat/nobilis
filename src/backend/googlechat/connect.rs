//! An account's connection: the session checked, the conversation list read, and the event stream kept open.
//!
//! 1. `get_self_user_status` says whose session this is (and fails at once when nobody's is).
//! 2. `paginated_world` lists the conversations; `get_members` names the people in them; each becomes a buffer.
//! 3. The BrowserChannel is opened and kept open ([`super::channel`]), registering again when it expires or the server
//!    forgets it. What arrived while it was away is caught up from the conversation list: a conversation whose latest
//!    activity moved has its newest history read again.
//!
//! A signed-out session ends the account's connection with a word to sign in again; it is not retried, since every
//! retry would be refused the same way and a loop of refused sign-ins is what gets a session flagged.

use super::api::Api;
use super::channel::{Channel, End};
use super::session::{GcError, Session};
use super::world::{self, World};
use super::{messages, pb, Group, Handle};
use crate::accounts::GoogleChatAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(5);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(300);
/// How often rotated cookies are written back to the account.
const SAVE_EVERY: Duration = Duration::from_secs(10 * 60);

enum Ending {
    SignedOut,
    Closed(String),
}

pub fn spawn(state: AppState, config: GoogleChatAccountConfig) {
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
        let Some(config) = state.accounts.get_googlechat(account_id) else { return };
        let started = Instant::now();
        let detail = match run_once(state, &config, account_id).await {
            Ok(Ending::SignedOut) => return signed_out(state, account_id),
            Err(e) if e.is_signed_out() => return signed_out(state, account_id),
            Ok(Ending::Closed(why)) => why,
            Err(e) => {
                tracing::warn!("googlechat[{account_id}]: {e}");
                e.to_string()
            }
        };
        state.runtime.clear_googlechat_handle(account_id);
        if started.elapsed() > Duration::from_secs(120) {
            delay = RECONNECT_INITIAL_DELAY;
        }
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        let wait = state.runtime.next_retry_delay(account_id, &mut delay, RECONNECT_INITIAL_DELAY, RECONNECT_MAX_DELAY);
        tracing::info!("googlechat[{account_id}]: disconnected ({detail}); reconnecting in {}s", wait.as_secs());
        state.runtime.report_retry(state, account_id, &detail, wait);
        tokio::time::sleep(wait).await;
    }
}

fn signed_out(state: &AppState, account_id: &str) {
    tracing::info!("googlechat[{account_id}]: signed out");
    state.runtime.clear_googlechat_handle(account_id);
    state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some("Google has signed this session out. Sign in to Google Chat again from Accounts."));
}

/// Saves cookies Google rotated, so the next start begins from the session as it now is.
fn save_cookies(state: &AppState, handle: &Handle) {
    if let Some(jar) = handle.api.session.take_rotated() {
        if let Err(e) = state.accounts.mutate_googlechat(&handle.account_id, |a| a.cookies = jar) {
            tracing::warn!("googlechat[{}]: could not save the session's cookies: {e:#}", handle.account_id);
        }
    }
}

async fn run_once(state: &AppState, config: &GoogleChatAccountConfig, account_id: &str) -> Result<Ending, GcError> {
    let router = crate::net::route::router();
    if router.routed(account_id) {
        router.ready(|msg| state.runtime.report_progress(state, account_id, msg)).await.map_err(|e| GcError::Network(format!("starting Tor or reaching the SOCKS5 proxy: {e:#}")))?;
    }
    let api = Arc::new(Api::new(Session::new(account_id, config.cookies.clone(), &config.user_agent)));
    let own_id = api.self_user_status().await?.user_status.and_then(|s| s.user_id).map(|u| u.id().to_string()).filter(|s| !s.is_empty()).ok_or_else(|| GcError::Network("Google Chat did not say whose session this is".into()))?;
    if !config.user_id.is_empty() && own_id != config.user_id {
        // The browser's first account is somebody else now. Not this account's session any more.
        tracing::warn!("googlechat[{account_id}]: the session belongs to another Google account");
        return Ok(Ending::SignedOut);
    }

    let handle = Handle { account_id: account_id.to_string(), api: api.clone(), world: Arc::new(Mutex::new(World { own_id: own_id.clone(), ..Default::default() })) };
    world::learn_people(&handle, vec![own_id.clone()]).await;
    let world_response = api.paginated_world().await?;
    let unknown = {
        let mut world = handle.world();
        if let Some(me) = world.people.get(&own_id).cloned() {
            world.own_name = me.name;
            world.own_email = me.email;
        }
        world::absorb_world(&mut world, &world_response);
        world.unknown_people()
    };
    world::learn_people(&handle, unknown).await;

    let (own_name, own_email) = {
        let world = handle.world();
        (world.own_name.clone(), world.own_email.clone())
    };
    if !own_name.is_empty() {
        state.runtime.set_own_identity(account_id, &own_name);
    }
    if (!own_name.is_empty() && own_name != config.name) || (!own_email.is_empty() && own_email != config.email) {
        let _ = state.accounts.mutate_googlechat(account_id, |a| {
            if !own_name.is_empty() {
                a.name = own_name.clone();
            }
            if !own_email.is_empty() {
                a.email = own_email.clone();
            }
        });
        state.events.emit("accountsChanged", serde_json::json!({}));
    }
    world::register_rail(state, &handle);
    let groups: Vec<Group> = handle.world().conversations.keys().cloned().collect();
    for group in &groups {
        world::register(state, &handle, group, false);
    }
    state.runtime.set_googlechat_handle(account_id, handle.clone());
    state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
    tracing::info!("googlechat[{account_id}]: connected ({} conversations)", groups.len());
    save_cookies(state, &handle);

    // Events are handled in order on a task of their own, so the stream is read while a name is being looked up.
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<pb::Event>();
    let worker = tokio::spawn({
        let state = state.clone();
        let handle = handle.clone();
        async move {
            let mut save = tokio::time::interval(SAVE_EVERY);
            save.tick().await;
            loop {
                tokio::select! {
                    event = events_rx.recv() => match event {
                        Some(event) => handle_event(&state, &handle, event).await,
                        None => break,
                    },
                    _ = save.tick() => save_cookies(&state, &handle),
                }
            }
        }
    });
    struct AbortOnDrop(tokio::task::AbortHandle);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _worker = AbortOnDrop(worker.abort_handle());

    let mut first = true;
    let mut quick_ends = 0u32;
    loop {
        if !first {
            catch_up(state, &handle).await?;
        }
        first = false;
        let started = Instant::now();
        let tx = events_tx.clone();
        let mut on_event = move |event: pb::Event| {
            let _ = tx.send(event);
        };
        let end = Channel::new(&api.session).run(&mut on_event, &mut || {}).await;
        save_cookies(state, &handle);
        // A channel that ends again straight away, again and again, is not going to work by registering faster.
        if started.elapsed() < Duration::from_secs(60) {
            quick_ends += 1;
        } else {
            quick_ends = 0;
        }
        match end {
            End::SignedOut => return Ok(Ending::SignedOut),
            End::Failed(why) => return Ok(Ending::Closed(why)),
            End::Expired | End::UnknownSid if quick_ends < 3 => {
                tracing::debug!("googlechat[{account_id}]: event stream ended ({end:?}); registering again");
                tokio::time::sleep(Duration::from_secs(1 << quick_ends)).await;
            }
            End::Expired | End::UnknownSid => return Ok(Ending::Closed("the event stream kept closing".into())),
        }
    }
}

/// After the stream was away: the conversation list read again, new conversations shown, and the newest history read
/// for every conversation that moved meanwhile.
async fn catch_up(state: &AppState, handle: &Handle) -> Result<(), GcError> {
    let response = handle.api.paginated_world().await?;
    let (moved, unknown) = {
        let mut world = handle.world();
        let before: std::collections::HashMap<Group, i64> = world.conversations.iter().map(|(g, c)| (g.clone(), c.sort_time)).collect();
        world::absorb_world(&mut world, &response);
        let moved: Vec<Group> = world.conversations.iter().filter(|(g, c)| before.get(*g).is_none_or(|t| c.sort_time > *t)).map(|(g, _)| g.clone()).collect();
        (moved, world.unknown_people())
    };
    world::learn_people(handle, unknown).await;
    for group in moved {
        let buffer = world::register(state, handle, &group, true);
        if let Err(e) = messages::history(state, handle, &buffer, false).await {
            tracing::debug!("googlechat[{}]: catching up: {e:#}", handle.account_id);
        }
    }
    Ok(())
}

async fn handle_event(state: &AppState, handle: &Handle, event: pb::Event) {
    use pb::event::event_body::Type;
    let kind = event.r#type();
    let group = Group::from_pb(event.group_id.as_ref());
    let Some(body) = event.body.and_then(|b| b.r#type) else { return };
    match body {
        Type::MessagePosted(posted) => {
            let (Some(group), Some(message)) = (group, posted.message) else { return };
            if kind == pb::event::EventType::MessageUpdated {
                // Edits are Round 3; the store already has the message by its id.
                return;
            }
            messages::record(state, handle, &group, &message).await;
        }
        Type::GroupReadStateUpdatedEvent(_) | Type::GroupViewed(_) => {}
        _ => {}
    }
}
