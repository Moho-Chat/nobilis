//! What the daemon asks of an account's thread, run beside the receive loop: each command gets its own local task and
//! a clone of the library's manager, so a slow send does not hold up receiving.

use super::ids;
use super::store::SignalStore;
use super::{Command, Handle};
use crate::state::AppState;
use presage::{
    libsignal_service::{content::ContentBody, prelude::ServiceError, protocol::ServiceId},
    manager::Registered,
    proto::{data_message, DataMessage, EditMessage},
    store::Thread,
    Error as PresageError, Manager,
};

type Library = Manager<SignalStore, Registered>;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// What a failed send means, in words a person can use.
pub fn explain(error: &PresageError<super::store::SignalStoreError>) -> String {
    use presage::libsignal_service::prelude::MessageSenderError as Sender;
    match error {
        PresageError::MessageSenderError(inner) => match inner.as_ref() {
            Sender::NotFound { .. } => "That person is not on Signal any more".to_string(),
            Sender::UntrustedIdentity { .. } => "Their safety number has changed. Check it with them in Signal before sending".to_string(),
            Sender::ServiceError(ServiceError::RateLimitExceeded { .. }) => "Signal asked for a pause: too many messages too quickly".to_string(),
            Sender::ServiceError(ServiceError::Unauthorized) => "Signal has unlinked this device".to_string(),
            other => format!("{other}"),
        },
        PresageError::UnknownGroup => "That group is not known to this account".to_string(),
        PresageError::ServiceError(ServiceError::RateLimitExceeded { .. }) => "Signal asked for a pause: too many messages too quickly".to_string(),
        other => format!("{other}"),
    }
}

/// Sends `body` to `thread`. The error is already in words.
async fn deliver(manager: &mut Library, thread: &Thread, body: impl Into<ContentBody>, at: u64) -> Result<(), String> {
    let sent = match thread {
        Thread::Contact(id) => manager.send_message(*id, body, at).await,
        Thread::Group(key) => manager.send_message_to_group(key, body, at).await,
    };
    sent.map_err(|e| {
        tracing::info!("signal: a send failed: {e}");
        explain(&e)
    })
}

fn finish(done: super::Done, result: Result<(), String>, at: u64) {
    let _ = done.send(result.map(|()| at));
}

pub fn run(_state: &AppState, handle: &Handle, manager: &Library, command: Command) {
    let account_id = handle.account_id.clone();
    let mut manager = manager.clone();
    tokio::task::spawn_local(async move {
        match command {
            Command::RefreshContacts => {
                if let Err(e) = manager.request_contacts().await {
                    tracing::warn!("signal[{account_id}]: asking the phone for its contacts: {e}");
                }
            }
            Command::Send { thread, body, ranges, quote, done } => {
                let at = now_ms();
                let dm = DataMessage { body: Some(body), body_ranges: ranges, quote, timestamp: Some(at), ..Default::default() };
                let result = deliver(&mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::React { thread, target_ms, target_author, emoji, remove, done } => {
                let at = now_ms();
                let reaction = data_message::Reaction {
                    emoji: Some(emoji),
                    remove: Some(remove),
                    target_author_aci: Some(target_author.to_string()),
                    target_author_aci_binary: Some(target_author.as_bytes().to_vec()),
                    target_sent_timestamp: Some(target_ms),
                };
                let dm = DataMessage { reaction: Some(reaction), timestamp: Some(at), ..Default::default() };
                let result = deliver(&mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::Delete { thread, target_ms, done } => {
                let at = now_ms();
                let dm = DataMessage { delete: Some(data_message::Delete { target_sent_timestamp: Some(target_ms) }), timestamp: Some(at), ..Default::default() };
                let result = deliver(&mut manager, &thread, dm, at).await;
                finish(done, result, at);
            }
            Command::Edit { thread, target_ms, body, ranges, done } => {
                let at = now_ms();
                let dm = DataMessage { body: Some(body), body_ranges: ranges, timestamp: Some(at), ..Default::default() };
                let edit = EditMessage { target_sent_timestamp: Some(target_ms), data_message: Some(dm) };
                let result = deliver(&mut manager, &thread, edit, at).await;
                finish(done, result, at);
            }
        }
    });
}

/// The service id of a person's thread, for the places that want one.
pub fn recipient(thread: &Thread) -> Option<ServiceId> {
    match thread {
        Thread::Contact(id) => Some(*id),
        Thread::Group(_) => None,
    }
}

/// The quote a reply to message `id` carries: who said it and what, as the daemon holds it.
pub fn quote_of(state: &AppState, buffer_id: &str, id: &str) -> Option<data_message::Quote> {
    let (ts, author) = ids::parse_message_id(id)?;
    let held = state.store.get_message(buffer_id, id).ok().flatten();
    Some(data_message::Quote {
        id: Some(ts),
        author_aci: Some(author.to_string()),
        author_aci_binary: Some(author.as_bytes().to_vec()),
        text: held.map(|m| m.body),
        ..Default::default()
    })
}
