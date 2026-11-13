//! Message requests: a conversation started by somebody the account has not accepted - a stranger, or a group the
//! account was put in by one. Signal's own clients show only the request until it is answered: the messages are kept,
//! nobody is told they were read, and the sender does not get the account's profile. moho does the same: the library
//! keeps the messages in the account's store, the window gets one entry on the Invitations tile, and accepting plays the
//! held messages into the conversation.
//!
//! Which conversations are requests is the storage service's to say (`storage::Kept::is_request`); until it has been
//! read nothing is held back, so a slow first read never hides a conversation.

use super::ids;
use super::people::{self, Ctx};
use super::storage;
use super::store::SignalStore;
use crate::state::AppState;
use presage::store::Thread;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// One conversation waiting for an answer.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Held {
    /// Who wrote first (a group's request is named by the group and says who added the account).
    pub from: String,
    /// What the first message said, shortened.
    pub preview: String,
    /// When the newest held message was sent (ms).
    pub at: u64,
    pub count: u32,
    pub group: bool,
}

const NAME: &str = "message-requests";

pub fn load(store: &SignalStore) -> BTreeMap<String, Held> {
    store.blob(NAME).ok().flatten().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn save(store: &SignalStore, held: &BTreeMap<String, Held>) {
    if let Ok(bytes) = serde_json::to_vec(held) {
        let _ = store.set_blob(NAME, &bytes);
    }
}

pub fn is_held(store: &SignalStore, remote: &str) -> bool {
    load(store).contains_key(remote)
}

/// Shortens what a message said to a line for the tile.
pub fn preview_of(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= 80 {
        return line;
    }
    let cut: String = line.chars().take(79).collect();
    format!("{cut}…")
}

/// Whether the account has said anything in the conversation (which answers a request as surely as accepting it).
pub async fn account_has_written(ctx: &Ctx<'_>, thread: &Thread) -> bool {
    use presage::store::ContentsStore;
    let Ok(messages) = ctx.store.messages(thread, ..).await else { return false };
    messages.filter_map(Result::ok).any(|m| ids::uuid_of(&m.metadata.sender) == ctx.own)
}

/// What to do with something arriving in `thread` from `author`.
#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    Show,
    /// Kept in the library's store, shown only as a request.
    Hold,
    /// From somebody (or in a group) the account blocked: not shown, not answered.
    Drop,
}

pub async fn gate(ctx: &Ctx<'_>, thread: &Thread, author: &Uuid) -> Gate {
    let kept = storage::current(ctx.account_id, &ctx.store);
    let remote = ids::remote_id(thread);
    let author_remote = format!("p:{author}");
    if kept.blocked(&remote) || (!ctx.is_own(author) && kept.blocked(&author_remote)) {
        return Gate::Drop;
    }
    if is_held(&ctx.store, &remote) {
        return Gate::Hold;
    }
    if ctx.is_own(author) {
        return Gate::Show;
    }
    let own_remote = format!("p:{}", ctx.own);
    // A group the storage service does not know yet is one the phone has not caught up with, not a stranger's.
    if matches!(thread, Thread::Group(_)) && kept.standing(&remote).is_none() {
        return Gate::Show;
    }
    // Cheap checks first: the store is only read when the storage service has not settled it.
    if !kept.is_request(&remote, &own_remote, false) {
        return Gate::Show;
    }
    if account_has_written(ctx, thread).await {
        return Gate::Show;
    }
    Gate::Hold
}

/// Notes a held message, and tells the window when the list changed.
pub async fn hold(ctx: &Ctx<'_>, thread: &Thread, author: &Uuid, at: u64, text: &str) {
    let remote = ids::remote_id(thread);
    let mut held = load(&ctx.store);
    let fresh = !held.contains_key(&remote);
    let entry = held.entry(remote).or_default();
    if fresh {
        entry.from = people::person(ctx, author).await;
        entry.preview = preview_of(text);
        entry.group = matches!(thread, Thread::Group(_));
    }
    entry.count += 1;
    entry.at = entry.at.max(at);
    save(&ctx.store, &held);
    if fresh {
        tracing::info!("signal[{}]: a message request arrived", ctx.account_id);
        announce(ctx).await;
    }
}

/// Takes a conversation off the list. Whether it was on it.
pub fn take(store: &SignalStore, remote: &str) -> bool {
    let mut held = load(store);
    let was = held.remove(remote).is_some();
    if was {
        save(store, &held);
    }
    was
}

/// The list as the window draws it: people as friend requests, groups as invitations.
pub async fn listing(ctx: &Ctx<'_>) -> serde_json::Value {
    let mut people_list = Vec::new();
    let mut groups = Vec::new();
    for (remote, held) in load(&ctx.store) {
        let Some(thread) = ids::parse_remote_id(&remote) else { continue };
        let more = if held.count > 1 { format!(" ({} messages)", held.count) } else { String::new() };
        match &thread {
            Thread::Contact(id) => {
                let who = ids::uuid_of(id);
                let name = people::person(ctx, &who).await;
                let detail = if held.preview.is_empty() { format!("wants to message you{more}") } else { format!("wants to message you: “{}”{more}", held.preview) };
                people_list.push(serde_json::json!({ "id": remote, "name": name, "avatarUrl": super::media::avatar_of(ctx.account_id, &who), "detail": detail }));
            }
            Thread::Group(key) => {
                groups.push(serde_json::json!({ "roomId": remote, "name": people::group_title(ctx, key).await, "avatarUrl": null, "inviter": held.from, "isDirect": false }));
            }
        }
    }
    serde_json::json!({ "accountId": ctx.account_id, "people": people_list, "groups": groups })
}

/// Tells the window what is waiting now.
pub async fn announce(ctx: &Ctx<'_>) {
    let listing = listing(ctx).await;
    ctx.state.events.emit("signalRequests", listing);
}

/// An answer to a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Accept,
    Delete,
    Block,
}

impl Answer {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "accept" => Some(Self::Accept),
            "delete" => Some(Self::Delete),
            "block" => Some(Self::Block),
            _ => None,
        }
    }

    /// The `MessageRequestResponse` the account's other devices are told.
    pub fn response_type(self) -> presage::proto::sync_message::message_request_response::Type {
        use presage::proto::sync_message::message_request_response::Type;
        match self {
            Self::Accept => Type::Accept,
            Self::Delete => Type::Delete,
            Self::Block => Type::BlockAndDelete,
        }
    }

    /// What an answer that arrived from the phone means here.
    pub fn from_response(kind: i32) -> Option<Self> {
        use presage::proto::sync_message::message_request_response::Type;
        match Type::try_from(kind).ok()? {
            Type::Accept => Some(Self::Accept),
            Type::Delete | Type::Spam => Some(Self::Delete),
            Type::Block | Type::BlockAndDelete | Type::BlockAndSpam => Some(Self::Block),
            Type::Unknown => None,
        }
    }
}

/// Applies an answer here, wherever it was given: the storage service's copy is amended until the phone's own catches
/// up, the held messages are played into the conversation or forgotten.
pub async fn apply(ctx: &Ctx<'_>, thread: &Thread, answer: Answer) {
    let remote = ids::remote_id(thread);
    storage::amend(ctx.account_id, &ctx.store, &remote, |s| match answer {
        Answer::Accept => {
            s.accepted = true;
            s.blocked = false;
        }
        Answer::Delete => {}
        Answer::Block => {
            s.blocked = true;
            s.accepted = false;
        }
    });
    let was_held = take(&ctx.store, &remote);
    match answer {
        Answer::Accept if was_held => replay(ctx, thread).await,
        Answer::Accept => {}
        Answer::Delete | Answer::Block => {
            let mut store = ctx.store.clone();
            let _ = presage::store::ContentsStore::clear_thread(&mut store, thread).await;
        }
    }
    if was_held {
        announce(ctx).await;
    }
}

/// Plays the messages the library kept for a conversation into it, oldest first.
pub async fn replay(ctx: &Ctx<'_>, thread: &Thread) {
    use presage::libsignal_service::content::ContentBody;
    use presage::store::ContentsStore;
    let Ok(messages) = ctx.store.messages(thread, ..).await else { return };
    let messages: Vec<_> = messages.filter_map(Result::ok).collect();
    tracing::info!("signal[{}]: an accepted request brings {} messages", ctx.account_id, messages.len());
    for content in messages {
        let author = ids::uuid_of(&content.metadata.sender);
        let at = content.metadata.client_timestamp.timestamp_millis().max(0) as u64;
        match &content.body {
            ContentBody::DataMessage(dm) => super::inbox::data_message(ctx, thread, author, at, dm).await,
            ContentBody::EditMessage(edit) => super::inbox::edited(ctx, thread, author, edit).await,
            _ => {}
        }
    }
}

/// Whether the buffer for a conversation should be made: not for a held one.
pub fn may_open(store: &SignalStore, remote: &str) -> bool {
    !is_held(store, remote)
}

/// The listing for an account from outside its thread (the RPC).
pub async fn listing_for(state: &AppState, handle: &super::Handle) -> serde_json::Value {
    let ctx = Ctx::new(state, &handle.account_id, handle.store.clone(), handle.own);
    listing(&ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::proto::{storage_record, ContactRecord, StorageRecord};
    use presage::libsignal_service::protocol::{Aci, ServiceId};
    use presage::model::identity::OnNewIdentity;

    const OWN: &str = "99999999-9999-4999-8999-999999999999";
    const FRIEND: &str = "11111111-1111-4111-8111-111111111111";
    const STRANGER: &str = "22222222-2222-4222-8222-222222222222";
    const BLOCKED: &str = "33333333-3333-4333-8333-333333333333";

    fn contact(aci: &str, accepted: bool, blocked: bool) -> StorageRecord {
        StorageRecord { record: Some(storage_record::Record::Contact(ContactRecord { aci: aci.into(), whitelisted: accepted, blocked, ..Default::default() })) }
    }

    fn thread(aci: &str) -> Thread {
        Thread::Contact(ServiceId::Aci(Aci::from(Uuid::parse_str(aci).unwrap())))
    }

    #[test]
    fn a_long_message_is_cut_to_a_line() {
        assert_eq!(preview_of("hello\n  there"), "hello there");
        let long = "x".repeat(200);
        assert_eq!(preview_of(&long).chars().count(), 80);
        assert!(preview_of(&long).ends_with('…'));
    }

    #[test]
    fn the_phones_answers_are_read() {
        use presage::proto::sync_message::message_request_response::Type;
        assert_eq!(Answer::from_response(Type::Accept as i32), Some(Answer::Accept));
        assert_eq!(Answer::from_response(Type::BlockAndSpam as i32), Some(Answer::Block));
        assert_eq!(Answer::from_response(Type::Spam as i32), Some(Answer::Delete));
        assert_eq!(Answer::from_response(Type::Unknown as i32), None);
        assert_eq!(Answer::parse("block").unwrap().response_type(), Type::BlockAndDelete);
    }

    #[tokio::test]
    async fn strangers_are_held_friends_shown_the_blocked_dropped_and_nothing_before_the_storage_service_is_read() {
        let state = crate::backend::kick::testkit::simulated_daemon("signal-requests");
        let store = SignalStore::open_in_memory(OnNewIdentity::Trust).unwrap();
        let account = "signal:requests-test";
        storage::forget(account);
        let ctx = Ctx::new(&state, account, store.clone(), Uuid::parse_str(OWN).unwrap());
        let stranger = Uuid::parse_str(STRANGER).unwrap();
        assert_eq!(gate(&ctx, &thread(STRANGER), &stranger).await, Gate::Show, "not read yet: nothing is held back");
        storage::keep(account, &store, storage::Kept::from_records(4, &[contact(FRIEND, true, false), contact(BLOCKED, false, true)]));
        assert_eq!(gate(&ctx, &thread(FRIEND), &Uuid::parse_str(FRIEND).unwrap()).await, Gate::Show);
        assert_eq!(gate(&ctx, &thread(BLOCKED), &Uuid::parse_str(BLOCKED).unwrap()).await, Gate::Drop);
        assert_eq!(gate(&ctx, &thread(STRANGER), &stranger).await, Gate::Hold);
        assert_eq!(gate(&ctx, &thread(OWN), &Uuid::parse_str(OWN).unwrap()).await, Gate::Show, "Note to Self");
        let group = Thread::Group([5u8; 32]);
        assert_eq!(gate(&ctx, &group, &stranger).await, Gate::Show, "a group the phone has not listed yet");
        assert_eq!(gate(&ctx, &group, &Uuid::parse_str(BLOCKED).unwrap()).await, Gate::Drop, "a blocked person in a group");

        hold(&ctx, &thread(STRANGER), &stranger, 1000, "hi, it's me").await;
        hold(&ctx, &thread(STRANGER), &stranger, 2000, "are you there").await;
        let held = load(&store);
        let entry = held.get(&format!("p:{STRANGER}")).unwrap();
        assert_eq!((entry.count, entry.at, entry.preview.as_str()), (2, 2000, "hi, it's me"));
        let listed = listing(&ctx).await;
        assert_eq!(listed["people"][0]["id"], format!("p:{STRANGER}"));
        assert!(listed["people"][0]["detail"].as_str().unwrap().contains("(2 messages)"));

        apply(&ctx, &thread(STRANGER), Answer::Accept).await;
        assert!(load(&store).is_empty());
        assert_eq!(gate(&ctx, &thread(STRANGER), &stranger).await, Gate::Show, "accepted here, before the phone's copy says so");
        apply(&ctx, &thread(STRANGER), Answer::Block).await;
        assert_eq!(gate(&ctx, &thread(STRANGER), &stranger).await, Gate::Drop);
        storage::forget(account);
    }
}
