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
use grammers_client::tl;
use grammers_session::types::PeerId;

pub const DIRECT_MESSAGES: &str = "Direct Messages";
pub const BOTS: &str = "Bots";
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
        // A bot is a conversation with a program, not a person: its own heading (it is still a direct chat).
        Peer::User(user) if user.is_bot() => BOTS,
        Peer::User(_) => DIRECT_MESSAGES,
        Peer::Channel(_) => CHANNELS,
        _ => GROUPS,
    }
}

/// The glyph in front of a conversation's name: a horn for a broadcast channel, a robot for a bot.
pub fn name_icon_of(peer: &Peer) -> Option<&'static str> {
    match peer {
        Peer::Channel(channel) if channel.raw.broadcast => Some("campaign"),
        Peer::User(user) if user.is_bot() => Some("smart_toy"),
        _ => None,
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
    state.runtime.set_buffer_name_icon(state, &buffer.id, name_icon_of(peer));
    state.runtime.get_buffer(&buffer.id).unwrap_or(buffer)
}


/// What the window says in place of the message box where the account may not write: the words every service uses.
pub const NO_SEND_PERMISSION: &str = "You do not have permission to send messages in this channel.";

/// What decides whether an account may write in a group or a channel, read off what Telegram says about it.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Rights {
    /// A broadcast channel, where only those allowed to post may write.
    pub broadcast: bool,
    pub creator: bool,
    /// An administrator (of any kind) of this chat.
    pub admin: bool,
    /// An administrator who may post in a broadcast channel.
    pub may_post: bool,
    /// The account has left, or was removed.
    pub gone: bool,
    /// The chat was upgraded to a supergroup and no longer takes messages.
    pub deactivated: bool,
    /// This account in particular may not send.
    pub banned_here: bool,
    /// Nobody but administrators may send.
    pub members_banned: bool,
}

/// Why the account cannot write in a chat, or `None` when it can.
pub fn read_only_reason(r: &Rights) -> Option<&'static str> {
    if r.gone {
        return Some("You are not a member of this chat.");
    }
    if r.deactivated {
        return Some("This group was upgraded and no longer takes messages.");
    }
    let boss = r.creator || r.admin;
    if r.broadcast {
        return (!(r.creator || r.may_post)).then_some(NO_SEND_PERMISSION);
    }
    if r.banned_here || (r.members_banned && !boss) {
        return Some(NO_SEND_PERMISSION);
    }
    None
}

fn sending_banned(rights: &Option<tl::enums::ChatBannedRights>, now: i64) -> bool {
    match rights {
        Some(tl::enums::ChatBannedRights::Rights(b)) => b.send_messages && (b.until_date == 0 || i64::from(b.until_date) > now),
        None => false,
    }
}

fn is_admin(rights: &Option<tl::enums::ChatAdminRights>) -> bool {
    rights.is_some()
}

fn may_post(rights: &Option<tl::enums::ChatAdminRights>) -> bool {
    matches!(rights, Some(tl::enums::ChatAdminRights::Rights(a)) if a.post_messages)
}

fn rights_of_channel(c: &tl::types::Channel, now: i64) -> Rights {
    Rights {
        broadcast: c.broadcast,
        creator: c.creator,
        admin: is_admin(&c.admin_rights),
        may_post: may_post(&c.admin_rights),
        gone: c.left,
        deactivated: false,
        banned_here: sending_banned(&c.banned_rights, now),
        members_banned: sending_banned(&c.default_banned_rights, now),
    }
}

/// What the account may do in a conversation. Only meaningful for a peer read from the dialog list: one that arrived
/// with a message may be a "min" copy without the account's rights in it.
pub fn rights_of(peer: &Peer, now: i64) -> Option<Rights> {
    match peer {
        Peer::Channel(channel) => Some(rights_of_channel(&channel.raw, now)),
        Peer::Group(group) => Some(match &group.raw {
            tl::enums::Chat::Chat(c) => Rights {
                creator: c.creator,
                admin: is_admin(&c.admin_rights),
                gone: c.left,
                deactivated: c.deactivated,
                members_banned: sending_banned(&c.default_banned_rights, now),
                ..Default::default()
            },
            tl::enums::Chat::Channel(c) => rights_of_channel(c, now),
            // A chat that is gone, or that this account cannot see.
            _ => Rights { gone: true, ..Default::default() },
        }),
        Peer::User(user) if user.id().bare_id_unchecked() == 777000 => None,
        _ => None,
    }
}

/// The reason the window gives for a message box it does not show, for a conversation read from the dialog list.
pub fn read_only_for(peer: &Peer, now: i64) -> Option<&'static str> {
    if let Peer::User(user) = peer {
        // Telegram's own service account sends its login codes and takes no reply.
        return (user.id().bare_id_unchecked() == 777000).then_some("Telegram does not take replies here.");
    }
    rights_of(peer, now).and_then(|r| read_only_reason(&r))
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
    fn a_broadcast_channel_takes_messages_only_from_those_allowed_to_post() {
        let feed = Rights { broadcast: true, ..Default::default() };
        assert_eq!(read_only_reason(&feed), Some(NO_SEND_PERMISSION));
        assert_eq!(read_only_reason(&Rights { admin: true, ..feed.clone() }), Some(NO_SEND_PERMISSION), "an administrator who may not post is a reader here");
        assert_eq!(read_only_reason(&Rights { admin: true, may_post: true, ..feed.clone() }), None);
        assert_eq!(read_only_reason(&Rights { creator: true, ..feed }), None);
    }

    #[test]
    fn a_group_takes_messages_unless_this_account_or_every_member_is_banned_from_sending() {
        assert_eq!(read_only_reason(&Rights::default()), None);
        assert_eq!(read_only_reason(&Rights { banned_here: true, ..Default::default() }), Some(NO_SEND_PERMISSION));
        assert_eq!(read_only_reason(&Rights { members_banned: true, ..Default::default() }), Some(NO_SEND_PERMISSION));
        assert_eq!(read_only_reason(&Rights { members_banned: true, admin: true, ..Default::default() }), None, "administrators may still send where members may not");
        assert!(read_only_reason(&Rights { gone: true, ..Default::default() }).unwrap().contains("not a member"));
        assert!(read_only_reason(&Rights { deactivated: true, ..Default::default() }).unwrap().contains("upgraded"));
    }

    #[test]
    fn a_ban_on_sending_that_has_run_out_is_not_one() {
        use tl::types::ChatBannedRights as B;
        let banned = |until| Some(tl::enums::ChatBannedRights::Rights(B { send_messages: true, until_date: until, view_messages: false, send_media: false, send_stickers: false, send_gifs: false, send_games: false, send_inline: false, embed_links: false, send_polls: false, change_info: false, invite_users: false, pin_messages: false, manage_topics: false, send_photos: false, send_videos: false, send_roundvideos: false, send_audios: false, send_voices: false, send_docs: false, send_plain: false, edit_rank: false, manage_linked_peers: false, send_reactions: false }));
        assert!(sending_banned(&banned(0), 1000), "no end is for ever");
        assert!(sending_banned(&banned(2000), 1000));
        assert!(!sending_banned(&banned(500), 1000));
        assert!(!sending_banned(&None, 1000));
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
