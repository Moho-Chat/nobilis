//! A group or channel's own settings, and making a new one.
//!
//! What the settings dialog shows is read from Telegram each time it opens (the full chat: description, link,
//! auto-delete, slow mode, who may write), with what this account may change; each change is one request, and the
//! dialog reads it all again after. A name, description or picture changed here arrives back as Telegram's own
//! update, as it would from any app.
//!
//! A group made here is a supergroup, which is what Telegram's own apps make now: it is the kind that can ban, silence
//! and promote, and a small group would be turned into one the first time any of that was asked of it anyway.

use super::dialogs;
use super::peers;
use super::send::{explain, peer_for};
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::tl;
use grammers_session::types::{PeerKind, PeerRef};
use serde_json::{json, Value};

/// The lengths Telegram allows.
const TITLE_MAX: usize = 128;
const ABOUT_MAX: usize = 255;

/// The auto-delete periods Telegram's own apps offer, in seconds; 0 is off.
pub const AUTO_DELETE: [i32; 4] = [0, 86_400, 604_800, 2_678_400];
/// The slow-mode waits Telegram accepts, in seconds; 0 is off.
pub const SLOW_MODE: [i32; 7] = [0, 10, 30, 60, 300, 900, 3600];

/// A title, checked.
pub fn checked_title(title: &str) -> Result<String, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("A chat needs a name".to_string());
    }
    if title.chars().count() > TITLE_MAX {
        return Err(format!("A name can be at most {TITLE_MAX} characters"));
    }
    Ok(title.to_string())
}

/// A description, checked (empty takes it off).
pub fn checked_about(about: &str) -> Result<String, String> {
    let about = about.trim();
    if about.chars().count() > ABOUT_MAX {
        return Err(format!("A description can be at most {ABOUT_MAX} characters"));
    }
    Ok(about.to_string())
}

/// The @names in what somebody typed: commas, spaces and t.me links all allowed.
pub fn names_in(typed: &str) -> Vec<String> {
    typed
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|s| s.trim().trim_start_matches("https://").trim_start_matches("t.me/").trim_start_matches('@'))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Finds each person by @name, saying which could not be found.
async fn people(handle: &Handle, names: &[String]) -> (Vec<tl::enums::InputUser>, Vec<String>) {
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for name in names {
        match handle.client.resolve_username(name).await {
            Ok(Some(peer @ grammers_client::peer::Peer::User(_))) => match peer.to_ref().await {
                Ok(Some(r)) => found.push(r.into()),
                _ => missing.push(format!("@{name}")),
            },
            _ => missing.push(format!("@{name}")),
        }
    }
    (found, missing)
}

/// The people Telegram would not add, from its answer: their privacy settings keep them out.
fn not_added(invited: &tl::enums::messages::InvitedUsers) -> usize {
    let tl::enums::messages::InvitedUsers::Users(i) = invited;
    i.missing_invitees.len()
}

/// Adds people to a group (or subscribers to a channel), by @name. Answers what could not be done, in words, or
/// nothing when everybody was added.
async fn add_to(handle: &Handle, peer: PeerRef, names: &[String]) -> Result<Option<String>, String> {
    let (users, mut missing) = people(handle, names).await;
    let mut kept_out = 0;
    if !users.is_empty() {
        let invited = match peer.id.kind() {
            PeerKind::Channel => handle.client.invoke(&tl::functions::channels::InviteToChannel { channel: peer.into(), users }).await.map_err(|e| explain(&e))?,
            _ => {
                let mut last = None;
                for user in users {
                    last = Some(handle.client.invoke(&tl::functions::messages::AddChatUser { chat_id: peer.into(), user_id: user, fwd_limit: 100 }).await.map_err(|e| explain(&e))?);
                }
                last.ok_or_else(|| "nobody to add".to_string())?
            }
        };
        kept_out = not_added(&invited);
    }
    if kept_out > 0 {
        missing.push(format!("{kept_out} whose privacy settings keep them from being added (send them the link instead)"));
    }
    Ok(Some(missing).filter(|m| !m.is_empty()).map(|m| format!("Not added: {}", m.join(", "))))
}

/// Makes a group or a broadcast channel, adds the people named, and answers its buffer id (and what could not be
/// done).
pub async fn create(state: &AppState, account_id: &str, title: &str, broadcast: bool, about: &str, members: &str) -> Result<Value, String> {
    let title = checked_title(title)?;
    let about = checked_about(about)?;
    let handle = state.runtime.telegram_handle(account_id).ok_or_else(|| "that Telegram account is not connected".to_string())?;
    let updates = handle
        .client
        .invoke(&tl::functions::channels::CreateChannel { broadcast, megagroup: !broadcast, for_import: false, forum: false, title, about, geo_point: None, address: None, ttl_period: None })
        .await
        .map_err(|e| match &e {
            grammers_client::InvocationError::Rpc(r) if r.name == "CHANNELS_TOO_MUCH" => "This account is in as many groups and channels as Telegram allows".to_string(),
            _ => explain(&e),
        })?;
    let chat = match &updates {
        tl::enums::Updates::Updates(u) => u.chats.first().cloned(),
        tl::enums::Updates::Combined(u) => u.chats.first().cloned(),
        _ => None,
    }
    .ok_or_else(|| "Telegram made the chat but did not say which it is".to_string())?;
    let peer = grammers_client::peer::Peer::from_raw(&handle.client, chat);
    let peer_ref = peer.to_ref().await.ok().flatten().ok_or_else(|| "Telegram did not say how to reach the new chat".to_string())?;
    let note = if names_in(members).is_empty() { None } else { add_to(&handle, peer_ref, &names_in(members)).await? };
    dialogs::sync(state, account_id, &handle).await.map_err(|e| e.to_string())?;
    let buffer = peers::buffer_for(state, account_id, &peer);
    Ok(json!({ "bufferId": buffer.id, "note": note }))
}

/// The settings dialog's facts about one group or channel.
pub async fn info(state: &AppState, buffer: &Buffer) -> Result<Value, String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    if peer.id.kind() == PeerKind::User {
        return Err("a conversation with one person has no group settings".to_string());
    }
    let resolved = handle.client.resolve_peer(peer).await.map_err(|e| explain(&e))?;
    let broadcast = matches!(resolved, grammers_client::peer::Peer::Channel(_));
    let me = handle.client.get_me().await.map_err(|e| explain(&e))?;
    let rights = match me.to_ref().await {
        Ok(Some(me)) => handle.client.get_permissions(peer, me).await.ok(),
        _ => None,
    };
    let creator = rights.as_ref().is_some_and(|r| r.is_creator());
    let admin = creator || rights.as_ref().is_some_and(|r| r.is_admin());
    let full = match peer.id.kind() {
        PeerKind::Channel => handle.client.invoke(&tl::functions::channels::GetFullChannel { channel: peer.into() }).await,
        _ => handle.client.invoke(&tl::functions::messages::GetFullChat { chat_id: peer.into() }).await,
    }
    .map_err(|e| explain(&e))?;
    let tl::enums::messages::ChatFull::Full(full) = full;
    let (about, invite, ttl, slow, count, default_send) = match &full.full_chat {
        tl::enums::ChatFull::Full(c) => (c.about.clone(), c.exported_invite.clone(), c.ttl_period, None, None, default_send_of(&full.chats, peer)),
        tl::enums::ChatFull::ChannelFull(c) => (c.about.clone(), c.exported_invite.clone(), c.ttl_period, c.slowmode_seconds, c.participants_count, default_send_of(&full.chats, peer)),
        tl::enums::ChatFull::CommunityFull(_) => return Err("a Telegram community has no group settings of its own".to_string()),
    };
    let link = invite.and_then(|i| match i {
        tl::enums::ExportedChatInvite::ChatInviteExported(i) if !i.revoked => Some(i.link),
        _ => None,
    });
    Ok(json!({
        "title": peers::title_of(&resolved),
        "about": about,
        "broadcast": broadcast,
        "supergroup": peer.id.kind() == PeerKind::Channel,
        "username": resolved.username(),
        "memberCount": count,
        "isAdmin": admin,
        "isCreator": creator,
        // Offered to administrators; Telegram refuses what a particular one lacks the right for, in words. Anybody may
        // add people to a group unless its administrators said otherwise.
        "mayEdit": admin,
        "mayInvite": admin || !broadcast,
        "mayBan": admin && peer.id.kind() == PeerKind::Channel,
        "link": link,
        "autoDelete": ttl.unwrap_or(0),
        "slowMode": slow.unwrap_or(0),
        "membersMaySend": default_send,
    }))
}

/// Whether ordinary members may send messages, from the chat's default rights.
fn default_send_of(chats: &[tl::enums::Chat], peer: PeerRef) -> bool {
    let id = peer.id.bare_id_unchecked();
    let rights = chats.iter().find_map(|c| match c {
        tl::enums::Chat::Chat(c) if c.id == id => Some(c.default_banned_rights.clone()),
        tl::enums::Chat::Channel(c) if c.id == id => Some(c.default_banned_rights.clone()),
        _ => None,
    });
    match rights.flatten() {
        Some(tl::enums::ChatBannedRights::Rights(r)) => !r.send_messages,
        None => true,
    }
}

/// The people banned from a supergroup or channel, for the dialog to lift a ban from.
pub async fn banned(state: &AppState, buffer: &Buffer) -> Result<Value, String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    if peer.id.kind() != PeerKind::Channel {
        return Ok(json!([]));
    }
    let mut iter = handle.client.iter_participants(peer).filter(tl::enums::ChannelParticipantsFilter::ChannelParticipantsKicked(tl::types::ChannelParticipantsKicked { q: String::new() }));
    let mut out = Vec::new();
    while out.len() < 200 {
        let Some(found) = iter.next().await.map_err(|e| explain(&e))? else { break };
        let peer = grammers_client::peer::Peer::User(found.user.clone());
        out.push(json!({ "userId": found.user.id().bare_id_unchecked().to_string(), "nick": super::messages::name_of(&peer) }));
    }
    Ok(Value::Array(out))
}

/// One change to a chat's settings, named by `action`. Answers a note when part of it could not be done.
pub async fn change(state: &AppState, buffer: &Buffer, action: &str, params: &Value) -> Result<Option<String>, String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    let text = |k: &str| params.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let number = |k: &str| params.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let channel = peer.id.kind() == PeerKind::Channel;
    let none = Ok(None);
    match action {
        "rename" => {
            let title = checked_title(&text("title"))?;
            if channel {
                handle.client.invoke(&tl::functions::channels::EditTitle { channel: peer.into(), title }).await.map_err(|e| explain(&e))?;
            } else {
                handle.client.invoke(&tl::functions::messages::EditChatTitle { chat_id: peer.into(), title }).await.map_err(|e| explain(&e))?;
            }
            none
        }
        "describe" => {
            let about = checked_about(&text("about"))?;
            handle.client.invoke(&tl::functions::messages::EditChatAbout { peer: peer.into(), about }).await.map_err(|e| match &e {
                grammers_client::InvocationError::Rpc(r) if r.name == "CHAT_ABOUT_NOT_MODIFIED" => "That is already the description".to_string(),
                _ => explain(&e),
            })?;
            none
        }
        "picture" => {
            let path = text("path");
            let photo = if path.is_empty() {
                tl::enums::InputChatPhoto::Empty
            } else {
                let uploaded = handle.client.upload_file(&path).await.map_err(|e| format!("could not read that picture: {e}"))?;
                tl::types::InputChatUploadedPhoto { file: Some(uploaded.raw), video: None, video_start_ts: None, video_emoji_markup: None }.into()
            };
            if channel {
                handle.client.invoke(&tl::functions::channels::EditPhoto { channel: peer.into(), photo }).await.map_err(|e| explain(&e))?;
            } else {
                handle.client.invoke(&tl::functions::messages::EditChatPhoto { chat_id: peer.into(), photo }).await.map_err(|e| explain(&e))?;
            }
            none
        }
        // A new primary link; the old one stops working.
        "resetLink" => {
            handle
                .client
                .invoke(&tl::functions::messages::ExportChatInvite { legacy_revoke_permanent: true, request_needed: false, peer: peer.into(), expire_date: None, usage_limit: None, title: None, subscription_pricing: None })
                .await
                .map_err(|e| explain(&e))?;
            none
        }
        "add" => add_to(&handle, peer, &names_in(&text("names"))).await,
        "autoDelete" => {
            let period = number("seconds");
            if !AUTO_DELETE.contains(&period) {
                return Err("Telegram deletes messages after a day, a week or a month".to_string());
            }
            handle.client.invoke(&tl::functions::messages::SetHistoryTtl { peer: peer.into(), period }).await.map_err(|e| explain(&e))?;
            none
        }
        "slowMode" => {
            let seconds = number("seconds");
            if !channel || !SLOW_MODE.contains(&seconds) {
                return Err("Slow mode is for a supergroup, and one of Telegram's own waits".to_string());
            }
            handle.client.invoke(&tl::functions::channels::ToggleSlowMode { channel: peer.into(), seconds }).await.map_err(|e| match &e {
                grammers_client::InvocationError::Rpc(r) if r.name == "CHAT_NOT_MODIFIED" => "That is already the wait".to_string(),
                _ => explain(&e),
            })?;
            none
        }
        // Whether ordinary members may write; everything else they may do is left as it was.
        "membersMaySend" => {
            let may = params.get("may").and_then(|v| v.as_bool()).unwrap_or(true);
            let rights = tl::types::ChatBannedRights {
                view_messages: false,
                send_messages: !may,
                send_media: !may,
                send_stickers: !may,
                send_gifs: !may,
                send_games: !may,
                send_inline: !may,
                embed_links: !may,
                send_polls: !may,
                change_info: true,
                invite_users: false,
                pin_messages: true,
                manage_topics: true,
                send_photos: !may,
                send_videos: !may,
                send_roundvideos: !may,
                send_audios: !may,
                send_voices: !may,
                send_docs: !may,
                send_plain: !may,
                edit_rank: false,
                manage_linked_peers: false,
                send_reactions: false,
                until_date: 0,
            };
            handle
                .client
                .invoke(&tl::functions::messages::EditChatDefaultBannedRights { peer: peer.into(), banned_rights: rights.into() })
                .await
                .map_err(|e| match &e {
                    grammers_client::InvocationError::Rpc(r) if r.name == "CHAT_NOT_MODIFIED" => "That is already so".to_string(),
                    _ => explain(&e),
                })?;
            none
        }
        _ => Err("Telegram has no such setting".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_read_from_whatever_was_typed() {
        assert_eq!(names_in("@ada, bob  https://t.me/carol\nt.me/dan"), ["ada", "bob", "carol", "dan"]);
        assert!(names_in(" , ").is_empty());
    }

    #[test]
    fn titles_and_descriptions_keep_to_telegrams_lengths() {
        assert_eq!(checked_title("  Lunch club "), Ok("Lunch club".to_string()));
        assert!(checked_title("   ").is_err());
        assert!(checked_title(&"x".repeat(129)).is_err());
        assert_eq!(checked_about(""), Ok(String::new()), "an empty description takes it off");
        assert!(checked_about(&"é".repeat(256)).is_err());
        assert!(checked_about(&"é".repeat(255)).is_ok());
    }

    #[test]
    fn whether_members_may_write_is_read_from_the_chats_default_rights() {
        let peer = PeerRef { id: grammers_session::types::PeerId::chat_unchecked(5), auth: Default::default() };
        let chat = |send_blocked: Option<bool>| {
            tl::enums::Chat::Chat(tl::types::Chat {
                creator: false,
                left: false,
                deactivated: false,
                call_active: false,
                call_not_empty: false,
                noforwards: false,
                id: 5,
                title: "Lunch".into(),
                photo: tl::enums::ChatPhoto::Empty,
                participants_count: 2,
                date: 0,
                version: 1,
                migrated_to: None,
                admin_rights: None,
                default_banned_rights: send_blocked.map(|b| {
                    tl::types::ChatBannedRights {
                        view_messages: false,
                        send_messages: b,
                        send_media: false,
                        send_stickers: false,
                        send_gifs: false,
                        send_games: false,
                        send_inline: false,
                        embed_links: false,
                        send_polls: false,
                        change_info: false,
                        invite_users: false,
                        pin_messages: false,
                        manage_topics: false,
                        send_photos: false,
                        send_videos: false,
                        send_roundvideos: false,
                        send_audios: false,
                        send_voices: false,
                        send_docs: false,
                        send_plain: false,
                        edit_rank: false,
                        manage_linked_peers: false,
                        send_reactions: false,
                        until_date: 0,
                    }
                    .into()
                }),
            })
        };
        assert!(default_send_of(&[chat(None)], peer));
        assert!(default_send_of(&[chat(Some(false))], peer));
        assert!(!default_send_of(&[chat(Some(true))], peer));
        assert!(default_send_of(&[], peer), "a chat Telegram did not describe is not assumed closed");
    }
}
