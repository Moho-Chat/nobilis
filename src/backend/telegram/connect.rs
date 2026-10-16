//! An account's connection: open the session it saved, stay connected, and reconnect when Telegram or the network
//! lets go - without ever signing in afresh to get over a hiccup (see mod.rs on why).
//!
//! Modelled on Steam's loop (`steam/connect.rs run_with_retry`): a wait that grows while a connection falls over at
//! once and starts again after one that held, a long wait when Telegram says to slow down, and a stop - not a retry
//! - when Telegram has ended the session, because the same key can never work again.

use super::link::{self, Link};
use super::messages::{self, Own};
use super::peers;
use super::Handle;
use super::session::{self, FileSession};
use crate::accounts::TelegramAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{Context, Result};
use grammers_client::InvocationError;
use grammers_mtsender::UpdatesConfiguration;
use std::sync::Arc;
use std::time::Duration;

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(10);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(300);

/// How often a changed session is written.
const SAVE_EVERY: Duration = Duration::from_secs(5);

/// Ends the account's session at Telegram, so removing it here does not leave a session in the account's list of
/// devices, and takes the saved session away. Best effort: the saved one is gone either way.
pub async fn sign_out(state: &AppState, config: &TelegramAccountConfig) {
    let path = session::path_for(config.user_id);
    if let Ok(session) = FileSession::open(path.clone()) {
        let session = Arc::new(session);
        if session.has_key() {
            // The account was removed first, so its own key is the only route it has left: not Tor unless it was.
            match link::open(state, &config.account_id(), config.api_id, session.clone()).await {
                Ok(link) => {
                    if let Err(e) = link.client.sign_out().await {
                        tracing::info!("telegram[{}]: signing out at Telegram: {e}", config.account_id());
                    }
                    link.close().await;
                }
                Err(e) => tracing::info!("telegram[{}]: could not reach Telegram to sign out: {e:#}", config.account_id()),
            }
        }
        session.delete();
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// How a connection ended.
#[derive(Debug, PartialEq)]
pub enum Ending {
    /// Telegram ended the session (or it is gone): sign in again.
    SignInAgain(String),
    /// Telegram asked for a pause, or the network is down: try again after this.
    Later(Duration, String),
    /// The connection ended; try again after the usual wait.
    Closed,
}

/// What an error from Telegram means for the connection.
pub fn classify(error: &InvocationError) -> Ending {
    match error {
        InvocationError::Rpc(rpc) => {
            let name = rpc.name.as_str();
            if rpc.code == 401 || matches!(name, "AUTH_KEY_UNREGISTERED" | "SESSION_REVOKED" | "SESSION_EXPIRED" | "USER_DEACTIVATED" | "USER_DEACTIVATED_BAN" | "AUTH_KEY_DUPLICATED") {
                Ending::SignInAgain(super::login::explain(name))
            } else if name == "FLOOD_WAIT" || name == "FLOOD_PREMIUM_WAIT" {
                // The number of seconds Telegram asked for, and a little more.
                Ending::Later(Duration::from_secs(u64::from(rpc.value.unwrap_or(60)) + 5), format!("Telegram asked for a pause of {} seconds", rpc.value.unwrap_or(60)))
            } else {
                Ending::Closed
            }
        }
        _ => Ending::Closed,
    }
}

/// Starts (or restarts) an account's connection.
pub fn spawn(state: AppState, config: TelegramAccountConfig) {
    let account_id = config.account_id();
    crate::net::route::router().mark(&account_id, config.use_tor);
    state.runtime.reset_connection(&account_id);
    let handle = tokio::spawn({
        let account_id = account_id.clone();
        let state = state.clone();
        async move {
            run_with_retry(&state, &account_id).await;
            state.runtime.remove_task_handle(&account_id);
        }
    });
    state.runtime.insert_task_handle(&account_id, handle.abort_handle());
}

async fn run_with_retry(state: &AppState, account_id: &str) {
    let mut delay = RECONNECT_INITIAL_DELAY;
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    loop {
        // The account as it is now: removed while this slept, it is not reconnected.
        let Some(config) = state.accounts.get_telegram(account_id) else { return };
        let (detail, wait) = match run(state, &config, account_id).await {
            Ok(Ending::SignInAgain(why)) => {
                tracing::info!("telegram[{account_id}]: stopped: {why}");
                state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some(&format!("{why}. Sign in to Telegram again from Accounts.")));
                return;
            }
            Ok(Ending::Later(wait, why)) => (why, Some(wait)),
            Ok(Ending::Closed) => ("connection ended".to_string(), None),
            Err(e) => {
                tracing::warn!("telegram[{account_id}]: {e:#}");
                (format!("{e:#}"), None)
            }
        };
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        let wait = wait.unwrap_or_else(|| state.runtime.next_retry_delay(account_id, &mut delay, RECONNECT_INITIAL_DELAY, RECONNECT_MAX_DELAY));
        tracing::info!("telegram[{account_id}]: not connected ({detail}); trying again in {}s", wait.as_secs());
        state.runtime.report_retry(state, account_id, &detail, wait);
        tokio::time::sleep(wait).await;
    }
}

/// One go at being connected.
async fn run(state: &AppState, config: &TelegramAccountConfig, account_id: &str) -> Result<Ending> {
    let session = Arc::new(FileSession::open(session::path_for(config.user_id))?);
    if !session.has_key() {
        return Ok(Ending::SignInAgain("this account has no saved session".to_string()));
    }
    let mut link = link::open(state, account_id, config.api_id, session).await?;
    let ending = stay(state, config, account_id, &mut link).await;
    state.runtime.clear_telegram_handle(account_id);
    link.close().await;
    ending
}

async fn stay(state: &AppState, config: &TelegramAccountConfig, account_id: &str, link: &mut Link) -> Result<Ending> {
    match link.client.is_authorized().await {
        Ok(true) => {}
        Ok(false) => return Ok(Ending::SignInAgain("Telegram no longer accepts this session".to_string())),
        Err(e) => return Ok(classify(&e)).and_then(|ending| if ending == Ending::Closed { Err(anyhow::Error::from(e)).context("asking Telegram whether the session is good") } else { Ok(ending) }),
    }
    // The name and number the account has now, which are Telegram's to change.
    let mut own = Own { name: if config.name.is_empty() { config.phone.clone() } else { config.name.clone() }, user_id: config.user_id };
    if let Ok(me) = link.client.get_me().await {
        let (name, username) = (me.full_name(), me.username().map(str::to_string));
        if !name.is_empty() {
            own.name = name.clone();
        }
        let _ = state.accounts.mutate_telegram(account_id, |a| {
            a.name = name;
            a.username = username;
        });
    }
    state.runtime.set_own_identity(account_id, &own.name);
    let handle = Handle { client: link.client.clone(), session: link.session.clone(), own: own.clone(), names: Default::default() };
    state.runtime.set_telegram_handle(account_id, handle.clone());
    state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
    tracing::info!("telegram[{account_id}]: connected as {} ({})", config.user_id, config.username.as_deref().unwrap_or("no username"));
    // The conversations, in the background: the connection is up whether or not this succeeds.
    tokio::spawn({
        let (state, account_id, handle) = (state.clone(), account_id.to_string(), handle.clone());
        async move {
            if let Err(e) = super::dialogs::sync(&state, &account_id, &handle).await {
                tracing::warn!("telegram[{account_id}]: reading the conversations: {e:#}");
            }
        }
    });

    let updates = link.updates.take().context("the updates were already taken")?;
    let mut stream = link.client.stream_updates(updates, UpdatesConfiguration { catch_up: true }).await.map_err(|e| anyhow::anyhow!("starting to read updates: {e}"))?;
    let mut save = tokio::time::interval(SAVE_EVERY);
    loop {
        tokio::select! {
            _ = save.tick() => {
                if let Err(e) = link.session.save_if_dirty() {
                    tracing::warn!("telegram[{account_id}]: saving the session: {e:#}");
                }
            }
            update = stream.next() => match update {
                Ok(update) => handle_update(state, account_id, &handle, update),
                Err(e) => {
                    let _ = stream.sync_update_state().await;
                    return match classify(&e) {
                        Ending::Closed => Err(anyhow::Error::from(e)).context("reading updates"),
                        other => Ok(other),
                    };
                }
            },
        }
    }
}

/// What Telegram sent unasked: a message, an edit, something deleted.
fn handle_update(state: &AppState, account_id: &str, handle: &Handle, update: grammers_client::update::Update) {
    use grammers_client::update::Update;
    match update {
        Update::NewMessage(message) => {
            messages::record_named(state, account_id, &handle.own, &message, Some(&handle.names));
        }
        Update::Raw(raw) => super::updates::raw(state, account_id, handle, &raw.raw),
        Update::MessageEdited(message) => {
            if let Some(peer) = message.peer() {
                let buffer = peers::buffer_for(state, account_id, peer);
                let body = messages::body_of(&message);
                let attachments = buffer.remote_id.as_deref().map(|remote| super::media::attachments_of(handle.own.user_id, remote, &message)).unwrap_or_default();
                state.runtime.update_message(state, &buffer.id, &message.id().to_string(), &body, &[], &attachments);
            }
        }
        Update::MessageDeleted(deleted) => {
            for id in deleted.messages() {
                forget_message(state, account_id, deleted.channel_id(), *id);
            }
        }
        _ => {}
    }
}

/// Removes a message Telegram says is gone. A channel's deletion names the channel; any other names only the number,
/// which is unique across the account's people and small groups, so the conversations are looked through.
fn forget_message(state: &AppState, account_id: &str, channel: Option<i64>, id: i32) {
    let msg_id = id.to_string();
    let candidates: Vec<crate::model::Buffer> = state
        .runtime
        .list_buffers()
        .into_iter()
        .filter(|b| b.account_id == account_id)
        .filter(|b| match (channel, b.remote_id.as_deref()) {
            (Some(c), Some(remote)) => remote == peers::remote_id(grammers_session::types::PeerId::channel_unchecked(c)),
            (Some(_), None) => false,
            (None, Some(remote)) => !remote.starts_with("-100"),
            (None, None) => false,
        })
        .collect();
    for buffer in candidates {
        if state.store.get_message(&buffer.id, &msg_id).ok().flatten().is_some() {
            state.runtime.delete_message(state, &buffer.id, &msg_id);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::RpcError;

    fn rpc(code: i32, name: &str, value: Option<u32>) -> InvocationError {
        InvocationError::Rpc(RpcError { code, name: name.to_string(), value, caused_by: None })
    }

    #[test]
    fn a_session_telegram_ended_stops_the_connection_and_a_flood_wait_waits_as_long_as_asked() {
        assert!(matches!(classify(&rpc(401, "AUTH_KEY_UNREGISTERED", None)), Ending::SignInAgain(_)));
        assert!(matches!(classify(&rpc(401, "SESSION_REVOKED", None)), Ending::SignInAgain(_)));
        assert!(matches!(classify(&rpc(420, "FLOOD_WAIT", Some(120))), Ending::Later(d, _) if d == Duration::from_secs(125)));
        assert_eq!(classify(&rpc(500, "INTERDC_CALL_ERROR", None)), Ending::Closed);
        assert_eq!(classify(&InvocationError::Io(std::io::Error::other("reset"))), Ending::Closed);
    }
}
