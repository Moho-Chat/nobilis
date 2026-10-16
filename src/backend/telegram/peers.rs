//! A Telegram conversation as a buffer: its name, its kind, its heading in the list.
//!
//! People, groups, supergroups, channels and bots are all "peers". A person (or a bot) is a direct message; a group or
//! a channel is a channel. The list is headed Direct Messages, Groups and Channels, as Twitch's is headed Channels and
//! Direct Messages (see `runtime.rs fixed_category`).
//!
//! The buffer is named for the conversation, and the buffer's name is what its history is filed under, so it must not
//! change when somebody renames themselves: a buffer is found by the peer's own id (Telegram's "bot API dialog id",
//! kept as the buffer's `remote_id`), and keeps the name it was first given. Two people with the same name are told
//! apart by their `@username`, or failing that by the end of their number.

use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::peer::Peer;
use grammers_session::types::PeerId;

pub const DIRECT_MESSAGES: &str = "Direct Messages";
pub const GROUPS: &str = "Groups";
pub const CHANNELS: &str = "Channels";

/// The id Telegram's own tools (and the bot API) call a peer by: a person's number, minus a group's, and a
/// supergroup's or channel's with `-100` in front.
pub fn remote_id(peer: PeerId) -> String {
    peer.bot_api_dialog_id().map(|id| id.to_string()).unwrap_or_default()
}

/// The peer a buffer's `remote_id` names.
pub fn peer_id_of(remote_id: &str) -> Option<PeerId> {
    PeerId::from_bot_api_dialog_id(remote_id.parse().ok()?)
}

pub fn kind_of(peer: &Peer) -> &'static str {
    match peer {
        Peer::User(_) => "dm",
        _ => "channel",
    }
}

pub fn category_of(peer: &Peer) -> &'static str {
    match peer {
        Peer::User(_) => DIRECT_MESSAGES,
        Peer::Channel(_) => CHANNELS,
        _ => GROUPS,
    }
}

/// What the conversation is called: the person's name, the group's title, or "Saved Messages" for the account's own.
pub fn title_of(peer: &Peer) -> String {
    match peer {
        Peer::User(user) if user.is_self() => "Saved Messages".to_string(),
        Peer::User(user) => {
            let name = user.full_name();
            if !name.trim().is_empty() {
                name
            } else if let Some(username) = user.username() {
                username.to_string()
            } else {
                "Deleted account".to_string()
            }
        }
        other => other.name().filter(|n| !n.trim().is_empty()).unwrap_or("Unnamed chat").to_string(),
    }
}

/// A name for a buffer that no other conversation of the account has.
pub fn unique_name(taken: &[(String, Option<String>)], remote_id: &str, title: &str, username: Option<&str>) -> String {
    let used = |name: &str| taken.iter().any(|(n, r)| n == name && r.as_deref() != Some(remote_id));
    if !used(title) {
        return title.to_string();
    }
    if let Some(username) = username.filter(|u| !u.is_empty()) {
        let with = format!("{title} (@{username})");
        if !used(&with) {
            return with;
        }
    }
    let digits: String = remote_id.chars().filter(|c| c.is_ascii_digit()).collect();
    let tail = &digits[digits.len().saturating_sub(4)..];
    format!("{title} ·{tail}")
}

/// The conversation's buffer, made if it is not there yet.
pub fn buffer_for(state: &AppState, account_id: &str, peer: &Peer) -> Buffer {
    let remote = remote_id(peer.id());
    let mine: Vec<Buffer> = state.runtime.list_buffers().into_iter().filter(|b| b.account_id == account_id).collect();
    if let Some(found) = mine.iter().find(|b| b.remote_id.as_deref() == Some(remote.as_str())) {
        return found.clone();
    }
    let taken: Vec<(String, Option<String>)> = mine.iter().map(|b| (b.name.clone(), b.remote_id.clone())).collect();
    let name = unique_name(&taken, &remote, &title_of(peer), peer.username());
    let buffer = state.runtime.ensure_buffer(state, account_id, &name, kind_of(peer));
    state.runtime.set_buffer_remote_id(state, &buffer.id, &remote);
    state.runtime.set_buffer_category(state, &buffer.id, Some(category_of(peer)), 0);
    state.runtime.get_buffer(&buffer.id).unwrap_or(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_people_with_one_name_are_told_apart_and_a_rename_never_makes_a_second_buffer() {
        let taken = vec![("Anna".to_string(), Some("111".to_string()))];
        assert_eq!(unique_name(&taken, "222", "Anna", None), "Anna ·222");
        assert_eq!(unique_name(&taken, "222", "Anna", Some("anna_k")), "Anna (@anna_k)");
        // The same person asking again is not a clash with themselves.
        assert_eq!(unique_name(&taken, "111", "Anna", None), "Anna");
        assert_eq!(unique_name(&taken, "333", "Boris", None), "Boris");
        assert_eq!(unique_name(&taken, "123456789", "Anna", None), "Anna ·6789");
    }

    #[test]
    fn an_id_goes_to_the_buffer_and_back() {
        let user = PeerId::user_unchecked(5173409493);
        assert_eq!(remote_id(user), "5173409493");
        assert_eq!(peer_id_of("5173409493"), Some(user));
        let channel = PeerId::channel_unchecked(1234567890);
        assert_eq!(remote_id(channel), "-1001234567890");
        assert_eq!(peer_id_of("-1001234567890"), Some(channel));
        assert_eq!(peer_id_of("not a number"), None);
    }
}
