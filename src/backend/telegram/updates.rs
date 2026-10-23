//! What Telegram sends that is not a message: somebody typing, a conversation read on another of the account's apps.
//!
//! The library turns messages, edits and deletions into updates of their own; everything else arrives as the raw
//! update Telegram sent, which is read here.

use super::peers;
use super::Handle;
use crate::state::AppState;
use grammers_client::tl;
use grammers_session::types::PeerId;

/// How long somebody's "typing" lasts without another: Telegram sends one every few seconds while they type.
const TYPING_FOR_MS: u64 = 6000;

/// The buffer for a conversation named by Telegram's id for it, if there is one.
fn buffer_of(state: &AppState, account_id: &str, remote: &str) -> Option<crate::model::Buffer> {
    state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(remote))
}

fn is_typing(action: &tl::enums::SendMessageAction) -> bool {
    use tl::enums::SendMessageAction as A;
    !matches!(action, A::SendMessageCancelAction | A::SpeakingInGroupCallAction)
}

fn who(handle: &Handle, user: i64) -> Option<String> {
    if user == handle.own.user_id {
        return None;
    }
    handle.names.lock().unwrap_or_else(|e| e.into_inner()).get(&user).cloned()
}

fn say_typing(state: &AppState, account_id: &str, buffer: &crate::model::Buffer, name: String) {
    state.events.emit("typing", serde_json::json!({ "accountId": account_id, "bufferId": buffer.id, "nicks": [name], "expiresInMs": TYPING_FOR_MS }));
}

/// Marks a conversation read here when Telegram says it was read up to its newest message elsewhere.
fn read_up_to(state: &AppState, account_id: &str, remote: &str, max_id: i32) {
    let Some(buffer) = buffer_of(state, account_id, remote) else { return };
    let newest = state.store.newest_message_id(&buffer.id).ok().flatten().and_then(|id| id.parse::<i32>().ok()).unwrap_or(0);
    if max_id >= newest {
        state.events.emit("bufferRead", serde_json::json!({ "accountId": account_id, "bufferId": buffer.id }));
    }
}

/// A topic was read on another of the account's apps: up to the message it names.
fn topic_read(state: &AppState, account_id: &str, group: &str, topic: i32, max_id: i32) {
    let Some(buffer) = buffer_of(state, account_id, &super::topics::remote_of(group, topic)) else { return };
    let newest = state.store.newest_message_id(&buffer.id).ok().flatten().and_then(|id| id.parse::<i32>().ok()).unwrap_or(0);
    if super::topics::read_all(max_id, newest, super::topics::top_of(&buffer.id)) {
        state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
        state.events.emit("bufferRead", serde_json::json!({ "accountId": account_id, "bufferId": buffer.id }));
    }
}

/// The pins of a conversation changed: read them again, which says so to every window.
fn pins_changed(state: &AppState, account_id: &str, _handle: &Handle, remote: &str) {
    let Some(buffer) = buffer_of(state, account_id, remote) else { return };
    let state = state.clone();
    tokio::spawn(async move {
        let _ = super::pins::list(&state, &buffer).await;
    });
}

/// Handles one raw update.
pub fn raw(state: &AppState, account_id: &str, handle: &Handle, update: &tl::enums::Update) {
    use tl::enums::Update as U;
    match update {
        U::UserTyping(u) if is_typing(&u.action) => {
            if let (Some(buffer), Some(name)) = (buffer_of(state, account_id, &u.user_id.to_string()), who(handle, u.user_id)) {
                say_typing(state, account_id, &buffer, name);
            }
        }
        U::ChatUserTyping(u) if is_typing(&u.action) => {
            let user = PeerId::from(u.from_id.clone()).bare_id();
            if let (Some(buffer), Some(name)) = (buffer_of(state, account_id, &peers::remote_id(PeerId::chat_unchecked(u.chat_id))), user.and_then(|id| who(handle, id))) {
                say_typing(state, account_id, &buffer, name);
            }
        }
        U::ChannelUserTyping(u) if is_typing(&u.action) => {
            let user = PeerId::from(u.from_id.clone()).bare_id();
            // In a group with topics somebody types in one of them.
            let group = peers::remote_id(PeerId::channel_unchecked(u.channel_id));
            let found = buffer_of(state, account_id, &group).or_else(|| buffer_of(state, account_id, &super::topics::remote_of(&group, u.top_msg_id.unwrap_or(super::topics::GENERAL))));
            if let (Some(buffer), Some(name)) = (found, user.and_then(|id| who(handle, id))) {
                say_typing(state, account_id, &buffer, name);
            }
        }
        U::MessageReactions(u) => super::reactions::update(state, account_id, &peers::remote_id(PeerId::from(u.peer.clone())), u.msg_id, &u.reactions),
        U::MessagePoll(u) => super::polls::update(state, u),
        U::NotifySettings(u) => super::notify::changed(state, account_id, u),
        U::PinnedMessages(u) => pins_changed(state, account_id, handle, &peers::remote_id(PeerId::from(u.peer.clone()))),
        U::PinnedChannelMessages(u) => pins_changed(state, account_id, handle, &peers::remote_id(PeerId::channel_unchecked(u.channel_id))),
        U::ReadHistoryInbox(u) => read_up_to(state, account_id, &peers::remote_id(PeerId::from(u.peer.clone())), u.max_id),
        // The account's folders were made, changed, deleted or put in another order, on this client or another.
        U::DialogFilter(_) | U::DialogFilters | U::DialogFilterOrder(_) => super::folders::changed(state, account_id, handle),
        U::ReadChannelDiscussionInbox(u) => topic_read(state, account_id, &peers::remote_id(PeerId::channel_unchecked(u.channel_id)), u.top_msg_id, u.read_max_id),
        U::ReadChannelInbox(u) => read_up_to(state, account_id, &peers::remote_id(PeerId::channel_unchecked(u.channel_id)), u.max_id),
        _ => {}
    }
}
