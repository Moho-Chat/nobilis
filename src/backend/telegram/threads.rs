//! Replies in a thread, and the comments under a channel's post.
//!
//! A reply chain in a group is a thread the window already knows how to draw from the messages it has (the replies
//! name the message at the top, see `messages::record`). What it cannot have is what was said before this client read
//! it, and a channel's comments, which are messages in the discussion group linked to the channel: those are asked for
//! here when a thread is opened, and a comment is sent to that group as an answer to the post's copy in it.

use super::peers;
use super::messages;
use super::send::{explain, input_for, peer_for};
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::tl;
use grammers_session::types::{PeerAuth, PeerId, PeerRef};
use serde_json::{json, Value};
use std::collections::HashMap;

/// How many replies a thread is read to.
const LIMIT: i32 = 100;

fn user_names(users: &[tl::enums::User]) -> HashMap<i64, String> {
    users
        .iter()
        .filter_map(|u| match u {
            tl::enums::User::User(u) => {
                let name = [u.first_name.clone().unwrap_or_default(), u.last_name.clone().unwrap_or_default()].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" ");
                Some((u.id, if name.is_empty() { u.username.clone().unwrap_or_else(|| "Deleted account".to_string()) } else { name }))
            }
            _ => None,
        })
        .collect()
}

/// One of Telegram's own messages as the window reads a message.
pub fn message_json(message: &tl::enums::Message, names: &HashMap<i64, String>, chat_name: &str, buffer_id: &str, own_name: &str) -> Option<Value> {
    let tl::enums::Message::Message(m) = message else { return None };
    let from = if m.out {
        own_name.to_string()
    } else {
        match &m.from_id {
            Some(tl::enums::Peer::User(u)) => names.get(&u.user_id).cloned().unwrap_or_else(|| "Somebody".to_string()),
            _ => chat_name.to_string(),
        }
    };
    let mut body = m.message.clone();
    if let Some(label) = m.media.as_ref().and_then(super::messages::media_label) {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&label);
    }
    Some(json!({
        "id": m.id.to_string(),
        "bufferId": buffer_id,
        "from": from,
        "body": body,
        "ts": m.date,
        "kind": "chat",
        "isAction": false,
        "isHighlight": false,
        "edited": m.edit_date.is_some(),
        "isOwn": m.out,
    }))
}

/// A thread's replies, oldest first.
pub async fn fetch(state: &AppState, buffer: &Buffer, root_id: &str) -> Result<Vec<Value>, String> {
    let root: i32 = root_id.parse().map_err(|_| "that message has no thread".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let answer = handle
        .client
        .invoke(&tl::functions::messages::GetReplies { peer: peer.into(), msg_id: root, offset_id: 0, offset_date: 0, add_offset: 0, limit: LIMIT, max_id: 0, min_id: 0, hash: 0 })
        .await
        .map_err(|e| explain(&e))?;
    let (messages, users, chats) = match answer {
        tl::enums::messages::Messages::Messages(m) => (m.messages, m.users, m.chats),
        tl::enums::messages::Messages::Slice(m) => (m.messages, m.users, m.chats),
        tl::enums::messages::Messages::ChannelMessages(m) => (m.messages, m.users, m.chats),
        tl::enums::messages::Messages::NotModified(_) => (Vec::new(), Vec::new(), Vec::new()),
    };
    let names = user_names(&users);
    let mut out: Vec<Value> = messages
        .iter()
        .filter_map(|m| {
            // A comment is in the discussion group, not the channel: filed under that group's buffer when it is one.
            let discussion = match m {
                tl::enums::Message::Message(raw) => match &raw.peer_id {
                    tl::enums::Peer::Channel(c) if buffer.remote_id.as_deref() != Some(&peers::remote_id(PeerId::channel_unchecked(c.channel_id))) => {
                        state.runtime.list_buffers().into_iter().find(|b| b.account_id == buffer.account_id && b.remote_id.as_deref() == Some(&peers::remote_id(PeerId::channel_unchecked(c.channel_id)))).map(|b| b.id)
                    }
                    _ => None,
                },
                _ => None,
            };
            let chat = chats.first().map(|c| match c {
                tl::enums::Chat::Channel(c) => c.title.clone(),
                tl::enums::Chat::Chat(c) => c.title.clone(),
                _ => buffer.name.clone(),
            });
            message_json(m, &names, chat.as_deref().unwrap_or(&buffer.name), discussion.as_deref().unwrap_or(&buffer.id), &handle.own.name)
        })
        .collect();
    out.sort_by_key(|m| m["ts"].as_i64().unwrap_or(0));
    Ok(out)
}

/// Says something in a thread: under a channel's post it goes to the discussion group, as an answer to the post's copy
/// there; anywhere else it is an answer to the message the thread grew from.
pub async fn reply(state: &AppState, buffer: &Buffer, root_id: &str, body: &str) -> Result<(), String> {
    let post: i32 = root_id.parse().map_err(|_| "that message has no thread".to_string())?;
    let input = input_for(body)?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let found = match handle.client.invoke(&tl::functions::messages::GetDiscussionMessage { peer: peer.into(), msg_id: post }).await {
        Ok(tl::enums::messages::DiscussionMessage::Message(found)) => Some(found),
        // A message nobody has answered yet has no thread to be found in: it is answered like any other.
        Err(_) => None,
    };
    let target = found.as_ref().and_then(|found| {
        let Some(tl::enums::Message::Message(copy)) = found.messages.first() else { return None };
        let tl::enums::Peer::Channel(group) = &copy.peer_id else { return None };
        let hash = found.chats.iter().find_map(|c| match c {
            tl::enums::Chat::Channel(c) if c.id == group.channel_id => c.access_hash,
            _ => None,
        })?;
        Some((PeerRef { id: PeerId::channel_unchecked(group.channel_id), auth: PeerAuth::from_hash(hash) }, copy.id))
    });
    let (to, reply_to) = target.unwrap_or((peer, post));
    let sent = handle.client.send_message(to, input.reply_to(Some(reply_to))).await.map_err(|e| explain(&e))?;
    messages::record(state, &buffer.account_id, &handle.own, &sent);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(id: i32, out: bool, from: i64, text: &str) -> tl::enums::Message {
        tl::enums::Message::Message(tl::types::Message {
            out,
            mentioned: false,
            media_unread: false,
            silent: false,
            post: false,
            from_scheduled: false,
            legacy: false,
            edit_hide: false,
            pinned: false,
            noforwards: false,
            invert_media: false,
            offline: false,
            video_processing_pending: false,
            paid_suggested_post_stars: false,
            paid_suggested_post_ton: false,
            id,
            from_id: Some(tl::enums::Peer::User(tl::types::PeerUser { user_id: from })),
            from_boosts_applied: None,
            from_rank: None,
            peer_id: tl::enums::Peer::User(tl::types::PeerUser { user_id: from }),
            saved_peer_id: None,
            fwd_from: None,
            via_bot_id: None,
            via_business_bot_id: None,
            reply_to: None,
            date: 1790000000 + id,
            message: text.into(),
            media: None,
            reply_markup: None,
            entities: None,
            views: None,
            forwards: None,
            replies: None,
            edit_date: None,
            post_author: None,
            grouped_id: None,
            reactions: None,
            restriction_reason: None,
            ttl_period: None,
            quick_reply_shortcut_id: None,
            effect: None,
            factcheck: None,
            report_delivery_until_date: None,
            paid_message_stars: None,
            suggested_post: None,
            schedule_repeat_period: None,
            summary_from_language: None,
            guestchat_via_from: None,
            rich_message: None,
        })
    }

    #[test]
    fn a_reply_is_read_as_a_message_with_its_authors_name() {
        let names: HashMap<i64, String> = [(7, "Ann".to_string())].into_iter().collect();
        let got = message_json(&raw(5, false, 7, "hello"), &names, "Chat", "telegram:1|Chat", "Me").unwrap();
        assert_eq!((got["id"].as_str(), got["from"].as_str(), got["body"].as_str(), got["isOwn"].as_bool()), (Some("5"), Some("Ann"), Some("hello"), Some(false)));
        let mine = message_json(&raw(6, true, 1, "mine"), &names, "Chat", "telegram:1|Chat", "Me").unwrap();
        assert_eq!((mine["from"].as_str(), mine["isOwn"].as_bool()), (Some("Me"), Some(true)));
        assert!(message_json(&tl::enums::Message::Empty(tl::types::MessageEmpty { id: 1, peer_id: None }), &names, "c", "b", "m").is_none());
    }
}
