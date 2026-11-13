//! An account's connection: the library's receive loop on a thread of its own, kept up and made again - without ever
//! linking afresh to get over a hiccup, and stopping, not retrying, when Signal has unlinked the device.
//!
//! The shape is Telegram's and Stoat's: a wait that grows while a connection falls over at once and starts again after
//! one that held, the wait Signal asks for when it says to slow down, and a stop when the link is over.
//!
//! What the library does when it receives (presage's `receive_messages`): opens the authenticated and the
//! unauthenticated websockets, sets the account's attributes, counts the pre-keys on the server and uploads more when
//! they are short (EC, signed and last-resort Kyber), decrypts each envelope, keeps the session and group state it
//! learns of in the store, and acknowledges an envelope when it has been handled. What is added here is the
//! lifecycle around it, and the translation of what it hands back (`inbox`).

use super::memory::{Memory, Synced};
use super::store::{self, OnNewIdentity, SignalStore};
use super::{inbox, local, Command, Handle};
use crate::accounts::SignalAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use futures::StreamExt;
use presage::{libsignal_service::prelude::ServiceError, manager::Registered, model::messages::Received, Error as PresageError, Manager};
use std::time::Duration;
use tokio::sync::oneshot;

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(10);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(300);

/// How long Signal waits before it unlinks a device it has not heard from. Its own figure is "about" this, so the
/// message that mentions it says so.
const UNLINK_AFTER_DAYS: i64 = 30;

/// How a connection ended.
#[derive(Debug, PartialEq)]
pub enum Ending {
    /// Signal no longer knows this device: link again.
    LinkAgain(String),
    /// Something has to be done on the phone first (a challenge); try again after a long wait.
    Slow(Duration, String),
    /// Something about how the account is set up stops it, and retrying will not change that.
    Refused(String),
    /// The connection ended; try again after the usual wait.
    Closed(String),
}

/// What an error from the library means for the connection.
pub fn classify(error: &PresageError<super::store::SignalStoreError>, days_away: Option<i64>) -> Ending {
    match error {
        PresageError::ServiceError(ServiceError::Unauthorized) | PresageError::RelinkNecessary | PresageError::NotYetRegisteredError => {
            Ending::LinkAgain(unlinked_words(days_away))
        }
        PresageError::ServiceError(ServiceError::RateLimitExceeded { retry_after }) => {
            let wait = retry_after.and_then(|d| d.to_std().ok()).unwrap_or(Duration::from_secs(60)).clamp(Duration::from_secs(10), Duration::from_secs(3600));
            Ending::Slow(wait, "Signal asked this device to slow down".to_string())
        }
        PresageError::ServiceError(ServiceError::ProofRequiredError(_)) | PresageError::PushChallengeRequired | PresageError::CaptchaRequired => {
            // A challenge is answered in Signal's own app; moho does not answer captchas.
            Ending::Slow(Duration::from_secs(900), "Signal wants a challenge answered, which moho does not do: open Signal on your phone".to_string())
        }
        other => Ending::Closed(format!("{other}")),
    }
}

/// What to say when Signal has unlinked the device, with the thirty-day rule when that is likely to be why.
pub fn unlinked_words(days_away: Option<i64>) -> String {
    match days_away {
        Some(days) if days >= UNLINK_AFTER_DAYS - 2 => {
            format!("Signal has unlinked this device. It was not connected for {days} days, and Signal unlinks a device it has not heard from for about {UNLINK_AFTER_DAYS}")
        }
        _ => "Signal has unlinked this device".to_string(),
    }
}

/// Whole days since `then` (seconds since the epoch), `None` when it was never.
pub fn days_since(then: i64, now: i64) -> Option<i64> {
    (then > 0 && now >= then).then(|| (now - then) / 86_400)
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Starts (or restarts) an account's connection.
pub fn spawn(state: AppState, config: SignalAccountConfig) {
    let account_id = config.account_id();
    crate::net::route::router().mark(&account_id, config.use_tor);
    state.runtime.reset_connection(&account_id);
    // The thread stops when this sender is dropped, which is what aborting the task below does.
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let done = local::run("signal", {
        let (state, account_id) = (state.clone(), account_id.clone());
        move || async move {
            tokio::select! {
                _ = run_with_retry(&state, &account_id) => {}
                _ = stop_rx => {}
            }
        }
    });
    let task = tokio::spawn({
        let (state, account_id) = (state.clone(), account_id.clone());
        async move {
            let _keep = stop_tx;
            let _ = done.await;
            state.runtime.clear_signal_handle(&account_id);
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
        let Some(config) = state.accounts.get_signal(account_id) else { return };
        let away = days_since(config.last_connected, now_seconds());
        let ending = run(state, &config, account_id, away).await;
        state.runtime.clear_signal_handle(account_id);
        let (wait, detail) = match ending {
            Ending::LinkAgain(why) => {
                tracing::info!("signal[{account_id}]: stopped: {why}");
                state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some(&format!("{why}. Link Signal again from Accounts.")));
                return;
            }
            Ending::Refused(why) => {
                tracing::info!("signal[{account_id}]: not connecting: {why}");
                state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some(&why));
                return;
            }
            Ending::Slow(wait, why) => {
                // Said once in words, then waited out: a challenge is not something retrying clears.
                tracing::info!("signal[{account_id}]: {why}");
                (wait, why)
            }
            Ending::Closed(why) => {
                tracing::warn!("signal[{account_id}]: {why}");
                let wait = state.runtime.next_retry_delay(account_id, &mut delay, RECONNECT_INITIAL_DELAY, RECONNECT_MAX_DELAY);
                (wait, why)
            }
        };
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        tracing::info!("signal[{account_id}]: not connected; trying again in {}s", wait.as_secs());
        state.runtime.report_retry(state, account_id, &detail, wait);
        tokio::time::sleep(wait).await;
    }
}

/// One go at being connected.
async fn run(state: &AppState, config: &SignalAccountConfig, account_id: &str, days_away: Option<i64>) -> Ending {
    // Through the account's route, if it has one; never directly instead when the route is not ready.
    if let Err(e) = super::network::prepare(crate::net::route::router().routed(account_id) || config.use_tor).await {
        return Ending::Closed(format!("the route for this account is not ready: {e}"));
    }
    let path = store::path_for(&config.aci);
    if !path.exists() {
        return Ending::LinkAgain("This account's keys are not on this computer".to_string());
    }
    let store = match SignalStore::open(&path, OnNewIdentity::Trust) {
        Ok(store) => store,
        Err(e) => return Ending::Closed(format!("opening the account's store: {e}")),
    };
    let mut manager: Manager<SignalStore, Registered> = match Manager::load_registered(store).await {
        Ok(manager) => manager,
        Err(e) => return classify(&e, days_away),
    };
    let stream = match manager.receive_messages().await {
        Ok(stream) => stream,
        Err(e) => return classify(&e, days_away),
    };
    futures::pin_mut!(stream);

    // Who this account's own messages are signed as, and the groups it is in, before anything is heard.
    inbox::announce_self(state, config, &manager).await;
    inbox::open_groups(state, account_id, &manager).await;

    let (commands, mut incoming) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let own = uuid::Uuid::from(manager.registration_data().service_ids.aci());
    let memory = std::sync::Arc::new(Memory::default());
    let synced = Synced::load(manager.store());
    memory.set_synced(synced);
    super::network::allow_link_previews(synced.link_previews == Some(true));
    let handle = Handle { account_id: account_id.to_string(), own, store: manager.store().clone(), memory, commands };
    state.runtime.set_signal_handle(account_id, handle.clone());
    state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
    tracing::info!("signal[{account_id}]: connected");
    let _ = state.accounts.mutate_signal(account_id, |a| a.last_connected = now_seconds());
    // The account's own name, for what it says from here.
    let _ = handle.commands.send(Command::Profile { who: own });
    let mut caught_up = false;
    let mut last_stamp = now_seconds();
    // What fell due while the daemon was away goes first, then the clock keeps watch.
    super::expiry::sweep(state, manager.store()).await;
    let mut sweeping = tokio::time::interval(Duration::from_secs(20));
    sweeping.tick().await;

    loop {
        tokio::select! {
            item = stream.next() => {
                let Some(item) = item else { return Ending::Closed("the connection ended".to_string()) };
                match item {
                    Received::QueueEmpty => {
                        // Everything that was waiting has been handled: what is sent from here on is built on
                        // sessions that are up to date (the library asks for exactly this before sending).
                        if !caught_up {
                            caught_up = true;
                            tracing::info!("signal[{account_id}]: caught up");
                            // The phone's privacy settings decide whether receipts and typing notices go out; until they
                            // are known nothing of the sort is sent.
                            if !handle.memory.synced().known() {
                                let _ = handle.commands.send(Command::AskPhone(presage::proto::sync_message::request::Type::Configuration));
                            }
                        }
                    }
                    Received::Contacts => inbox::contacts_arrived(state, &handle, &manager).await,
                    Received::Content(content) => inbox::content(state, &handle, &manager, *content).await,
                    Received::DecryptionError(sender) => inbox::decryption_failed(state, &handle, &mut manager, sender).await,
                }
                // A device that is up stays recorded as seen, a day at a time at most so the file is not rewritten.
                let now = now_seconds();
                if now - last_stamp >= 86_400 {
                    last_stamp = now;
                    let _ = state.accounts.mutate_signal(account_id, |a| a.last_connected = now);
                }
            }
            _ = sweeping.tick() => {
                super::expiry::sweep(state, manager.store()).await;
            }
            command = incoming.recv() => {
                match command {
                    Some(command) => super::commands::run(state, &handle, &manager, command),
                    None => return Ending::Closed("the account was disconnected".to_string()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::store::SignalStoreError;

    type Error = PresageError<SignalStoreError>;

    #[test]
    fn an_unauthorized_connection_is_an_unlink_and_stops_the_account() {
        let ending = classify(&Error::ServiceError(ServiceError::Unauthorized), None);
        assert_eq!(ending, Ending::LinkAgain("Signal has unlinked this device".to_string()));
        assert!(matches!(classify(&Error::NotYetRegisteredError, None), Ending::LinkAgain(_)));
    }

    #[test]
    fn a_long_absence_is_named_as_the_likely_reason() {
        let Ending::LinkAgain(words) = classify(&Error::ServiceError(ServiceError::Unauthorized), Some(31)) else { panic!() };
        assert!(words.contains("31 days") && words.contains("about 30"), "{words}");
        let Ending::LinkAgain(short) = classify(&Error::ServiceError(ServiceError::Unauthorized), Some(3)) else { panic!() };
        assert!(!short.contains("days"), "{short}");
    }

    #[test]
    fn a_rate_limit_waits_as_long_as_signal_asked_within_reason() {
        let asked = |s: i64| classify(&Error::ServiceError(ServiceError::RateLimitExceeded { retry_after: Some(chrono::Duration::seconds(s)) }), None);
        assert!(matches!(asked(90), Ending::Slow(w, _) if w == Duration::from_secs(90)));
        assert!(matches!(asked(1), Ending::Slow(w, _) if w == Duration::from_secs(10)));
        assert!(matches!(asked(86_400), Ending::Slow(w, _) if w == Duration::from_secs(3600)));
        let unspecified = classify(&Error::ServiceError(ServiceError::RateLimitExceeded { retry_after: None }), None);
        assert!(matches!(unspecified, Ending::Slow(w, _) if w == Duration::from_secs(60)));
    }

    #[test]
    fn a_challenge_is_left_to_the_phone_and_a_dropped_socket_is_retried() {
        assert!(matches!(classify(&Error::CaptchaRequired, None), Ending::Slow(w, why) if w >= Duration::from_secs(600) && why.contains("phone")));
        assert!(matches!(classify(&Error::MessagePipeInterruptedError, None), Ending::Closed(_)));
    }

    #[test]
    fn days_away_counts_whole_days_and_is_nothing_for_never() {
        assert_eq!(days_since(0, 1_000_000), None);
        assert_eq!(days_since(1_000_000, 1_000_000 + 3 * 86_400 + 5), Some(3));
        assert_eq!(days_since(2_000_000, 1_000_000), None);
    }
}
