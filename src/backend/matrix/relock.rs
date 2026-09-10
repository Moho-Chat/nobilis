//! Messages that could not be read, and reading them when the key turns up.
//!
//! A Megolm session routinely arrives after the messages it opens - key-share
//! to-device events lag the timeline, a room joined today has history from
//! before this device existed, and key backup restores a session minutes after
//! the messages needing it are already on screen. Until now every one of those
//! kept its placeholder forever: the message was written down as "unable to
//! decrypt" and nothing ever looked at it again, so a restored backup fixed
//! nothing that was already displayed and a restart was the only cure.
//!
//! Two halves, and both are needed. **Asking** - `request_room_key` to our own
//! other devices and the sender's, any of which may still hold the session.
//! And **retrying** - because a key can arrive without having been asked for,
//! from a backup restore, a key import, or somebody else's device deciding to
//! share.
//!
//! What is held is the encrypted event itself, because that is the only thing
//! that can be decrypted later; the placeholder on screen is a rendering of a
//! failure, not something that can be turned back into a message.

use super::*;
use crate::backend::matrix::crypto::CryptoSession;
use serde_json::Value;
use std::collections::HashMap;

/// How many unreadable messages to keep hold of per account.
///
/// Joining a large encrypted room with no keys at all produces thousands of
/// these at once, and holding every one would mean holding the whole room's
/// ciphertext in memory to no purpose - what the reader can see is a screen's
/// worth, and a session that opens one message opens all of its neighbours.
/// Oldest are dropped first.
const MAX_PENDING: usize = 500;

/// One message waiting for its key.
#[derive(Clone)]
pub struct Locked {
    pub buffer_id: String,
    pub room_id: String,
    pub event_id: String,
    /// The encrypted event exactly as it arrived, which is the only form that
    /// can be decrypted later.
    pub event: Value,
}

fn pending() -> &'static std::sync::Mutex<HashMap<String, Vec<Locked>>> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Vec<Locked>>>> = std::sync::OnceLock::new();
    PENDING.get_or_init(Default::default)
}

/// Remembers a message that could not be read, and asks for its key.
///
/// Best-effort on both counts. A key request that cannot be sent is not worth
/// failing a sync over, and the retry below covers the case where nobody ever
/// answers it but a key arrives another way.
pub async fn note_locked(
    session: &CryptoSession,
    homeserver_url: &str,
    access_token: &str,
    account_id: &str,
    buffer_id: &str,
    room_id: &str,
    event_id: &str,
    event: &Value,
) {
    {
        let mut all = pending().lock().unwrap();
        let waiting = all.entry(account_id.to_string()).or_default();
        // The same event twice is the same event: a sync that overlaps what
        // was already seen would otherwise ask for one key repeatedly.
        if waiting.iter().any(|l| l.event_id == event_id) {
            return;
        }
        waiting.push(Locked {
            buffer_id: buffer_id.to_string(),
            room_id: room_id.to_string(),
            event_id: event_id.to_string(),
            event: event.clone(),
        });
        if waiting.len() > MAX_PENDING {
            let over = waiting.len() - MAX_PENDING;
            waiting.drain(..over);
        }
    }

    let Ok(room) = ruma_common::RoomId::parse(room_id) else { return };
    if let Err(e) = session.request_room_key(homeserver_url, access_token, event, &room).await {
        tracing::debug!("matrix[{account_id}]: asking for the key to {event_id}: {e:#}");
    }
}

/// Tries every message still waiting, and rewrites the ones that open.
///
/// Called after each sync's crypto changes are taken in, and after a key
/// backup restore or an import - the three ways a session can appear. Cheap
/// when there is nothing waiting, which is the ordinary case.
pub async fn retry_locked(state: &AppState, session: &CryptoSession, account_id: &str) {
    let waiting = {
        let all = pending().lock().unwrap();
        match all.get(account_id) {
            Some(waiting) if !waiting.is_empty() => waiting.clone(),
            _ => return,
        }
    };

    let mut opened: Vec<String> = Vec::new();
    for locked in &waiting {
        let Ok(room) = ruma_common::RoomId::parse(&locked.room_id) else {
            opened.push(locked.event_id.clone());
            continue;
        };
        let Ok(decrypted) = crypto::decrypt_room_event(session, &locked.event, &room).await else { continue };
        // Only what a message is. A reaction or an edit that could not be
        // read is dropped rather than rewritten: there is no placeholder on
        // screen for either, so there is nothing to correct, and replaying
        // them into the timeline now would apply them out of order.
        let content = &decrypted["content"];
        let (body, _) = protocol::message_body(content);
        if !body.is_empty() {
            state.runtime.update_message(state, &locked.buffer_id, &locked.event_id, &body, &[], &[]);
        }
        opened.push(locked.event_id.clone());
    }

    if opened.is_empty() {
        return;
    }
    tracing::debug!("matrix[{account_id}]: {} message(s) became readable", opened.len());
    let mut all = pending().lock().unwrap();
    if let Some(waiting) = all.get_mut(account_id) {
        waiting.retain(|l| !opened.contains(&l.event_id));
    }
}

// Deliberately no way to forget an account's waiting list. Holding it across
// a disconnect is what makes a reconnect useful: the events are still
// unreadable, the keys may arrive on the next sync or from a backup restored
// afterwards, and dropping them would mean a reconnect that fixes nothing.
// The list is bounded and empties itself as messages open.

#[cfg(test)]
mod tests {
    use super::*;

    fn locked(id: &str) -> Locked {
        Locked {
            buffer_id: "matrix:me|!room".into(),
            room_id: "!room:example.org".into(),
            event_id: id.into(),
            event: serde_json::json!({ "type": "m.room.encrypted" }),
        }
    }

    /// Joining a large encrypted room with no keys produces thousands of
    /// these at once. Holding every one would mean holding the room's whole
    /// ciphertext to no purpose - a session that opens one message opens its
    /// neighbours, and what anybody can see is a screen's worth.
    #[test]
    fn the_oldest_are_dropped_rather_than_growing_without_end() {
        let mut waiting: Vec<Locked> = (0..MAX_PENDING + 50).map(|i| locked(&format!("${i}"))).collect();
        if waiting.len() > MAX_PENDING {
            let over = waiting.len() - MAX_PENDING;
            waiting.drain(..over);
        }
        assert_eq!(waiting.len(), MAX_PENDING);
        // The newest survive, which are the ones somebody is looking at.
        assert_eq!(waiting.last().unwrap().event_id, format!("${}", MAX_PENDING + 49));
        assert_eq!(waiting.first().unwrap().event_id, "$50");
    }

    /// A sync that overlaps what was already seen must not turn one
    /// unreadable message into a second key request for the same thing.
    #[test]
    fn the_same_message_is_only_waited_on_once() {
        let mut waiting = vec![locked("$a")];
        for id in ["$a", "$b", "$a"] {
            if !waiting.iter().any(|l| l.event_id == id) {
                waiting.push(locked(id));
            }
        }
        assert_eq!(waiting.len(), 2);
    }
}
