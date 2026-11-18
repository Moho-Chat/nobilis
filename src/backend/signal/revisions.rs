//! Edited messages have two names. moho knows a message by when it was first sent (`<ms>-<aci>`), and an edit names
//! the message that way too; but a reaction, a quote or a delete made after an edit names it by when its newest edit was
//! sent, as Signal's clients keep the newest revision as the message. Found on the first real account: a heart put on
//! an edited note on the phone was looked for under the edit's time and drawn nowhere.
//!
//! So each edit is noted, both ways: the revision's name leads to the message, and the message knows its newest
//! revision's time, which is what moho names it by when it reacts, quotes or deletes.

use super::ids;
use super::store::SignalStore;
use uuid::Uuid;

fn revision_key(revision_id: &str) -> String {
    format!("revision:{revision_id}")
}

fn latest_key(original_id: &str) -> String {
    format!("latest:{original_id}")
}

/// An edit of `original_id` by `author`, sent at `revision_ms`.
pub fn note(store: &SignalStore, author: &Uuid, revision_ms: u64, original_id: &str) {
    let revision = ids::message_id(revision_ms, author);
    if revision == original_id {
        return;
    }
    let _ = store.set_blob(&revision_key(&revision), original_id.as_bytes());
    // Only forward: an older revision arriving late (a device that was away) is not the newest.
    if revision_ms > latest_ms(store, original_id, 0) {
        let _ = store.set_blob(&latest_key(original_id), revision_ms.to_string().as_bytes());
    }
}

/// The message a name stands for: the message itself, or the one a revision's name belongs to.
pub fn original(store: &SignalStore, id: &str) -> String {
    store.blob(&revision_key(id)).ok().flatten().and_then(|b| String::from_utf8(b).ok()).unwrap_or_else(|| id.to_string())
}

/// When the newest revision of a message was sent: what to name it by in a reaction, a quote or a delete.
pub fn latest_ms(store: &SignalStore, original_id: &str, sent_ms: u64) -> u64 {
    store.blob(&latest_key(original_id)).ok().flatten().and_then(|b| String::from_utf8(b).ok()).and_then(|t| t.parse().ok()).unwrap_or(sent_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edited_message_is_found_by_its_newest_revision_and_named_by_it() {
        let store = SignalStore::open_in_memory(presage::model::identity::OnNewIdentity::Trust).unwrap();
        let me = Uuid::from_u128(1);
        let message = ids::message_id(100, &me);
        // Not edited: it is its own name, both ways.
        assert_eq!(original(&store, &message), message);
        assert_eq!(latest_ms(&store, &message, 100), 100);
        // Edited twice: either revision leads to it, and the newest names it.
        note(&store, &me, 200, &message);
        note(&store, &me, 300, &message);
        note(&store, &me, 250, &message);
        assert_eq!(original(&store, &ids::message_id(200, &me)), message);
        assert_eq!(original(&store, &ids::message_id(300, &me)), message);
        assert_eq!(latest_ms(&store, &message, 100), 300);
        // Another author's message sent at the same moment is not this one.
        assert_eq!(original(&store, &ids::message_id(300, &Uuid::from_u128(2))), ids::message_id(300, &Uuid::from_u128(2)));
    }
}
