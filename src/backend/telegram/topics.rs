//! Forum topics: a group with Topics switched on is several conversations, each its own place to read and write.
//!
//! Telegram keeps a topic as a message (its first, the "topic root") that every message in the topic answers; the
//! General topic is number 1. Each topic is a conversation of its own here, filed under its group's entry on the rail,
//! named `Group/#Topic`, and its `remote_id` is the group's with the topic after a colon (`-100123:45`). A message is
//! routed to its topic from what it answers; the topics come from the group when the account connects, and a topic
//! made while moho is running is found when its first message arrives.

use super::peers;
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use grammers_client::peer::Peer;
use grammers_client::tl;

/// The topic every message with no other belongs to.
pub const GENERAL: i32 = 1;

/// Whether a conversation is a group with Topics switched on.
pub fn is_forum(peer: &Peer) -> bool {
    matches!(peer, Peer::Group(g) if matches!(&g.raw, tl::enums::Chat::Channel(c) if c.forum))
}

/// A topic's `remote_id`: its group's, then the topic.
pub fn remote_of(group_remote: &str, topic: i32) -> String {
    format!("{group_remote}:{topic}")
}

/// The topic a conversation is, when it is one.
pub fn topic_of_buffer(buffer: &Buffer) -> Option<i32> {
    buffer.remote_id.as_deref()?.split_once(':')?.1.parse().ok()
}

fn header_topic(header: &tl::enums::MessageReplyHeader) -> Option<i32> {
    let tl::enums::MessageReplyHeader::Header(h) = header else { return None };
    if !h.forum_topic {
        return None;
    }
    // A message in a topic answers the topic's first message, or - when it answers another - names the topic as well.
    h.reply_to_top_id.or(h.reply_to_msg_id)
}

/// The topic a message was said in.
pub fn topic_of(raw: &tl::enums::Message) -> i32 {
    match raw {
        tl::enums::Message::Message(m) => m.reply_to.as_ref().and_then(header_topic).unwrap_or(GENERAL),
        // The message that makes a topic is the topic's own root.
        tl::enums::Message::Service(s) if matches!(s.action, tl::enums::MessageAction::TopicCreate(_)) => s.id,
        tl::enums::Message::Service(s) => s.reply_to.as_ref().and_then(header_topic).unwrap_or(GENERAL),
        tl::enums::Message::Empty(_) => GENERAL,
    }
}

fn display_name(group: &str, title: &str) -> String {
    format!("{group}/#{}", title.trim())
}

/// The conversation of one topic, made if it is not there yet. `title` is its name where known; without it a topic
/// other than General is called "topic N" until the group is asked.
pub fn buffer_for_topic(state: &AppState, account_id: &str, peer: &Peer, topic: i32, title: Option<&str>) -> Buffer {
    let group_remote = peers::remote_id(peer.id());
    let remote = remote_of(&group_remote, topic);
    let mine: Vec<Buffer> = state.runtime.list_buffers().into_iter().filter(|b| b.account_id == account_id).collect();
    let group_title = peers::title_of(peer);
    if let Some(found) = mine.iter().find(|b| b.remote_id.as_deref() == Some(remote.as_str())) {
        // A name learned since: the conversation takes it.
        if let Some(title) = title {
            let wanted = display_name(&group_title, title);
            if found.name != wanted && !mine.iter().any(|b| b.name == wanted) {
                state.runtime.rename_buffer(state, account_id, &found.name, &wanted, "channel");
                if let Some(renamed) = state.runtime.get_buffer(&crate::model::buffer_id(account_id, &wanted)) {
                    return renamed;
                }
            }
        }
        return found.clone();
    }
    let title_here = match (title, topic) {
        (Some(t), _) => t.to_string(),
        (None, GENERAL) => "General".to_string(),
        (None, n) => format!("topic {n}"),
    };
    let taken: Vec<(String, Option<String>)> = mine.iter().map(|b| (b.name.clone(), b.remote_id.clone())).collect();
    let name = peers::unique_name(&taken, &remote, &display_name(&group_title, &title_here), None);
    let buffer = state.runtime.ensure_buffer(state, account_id, &name, "channel");
    state.runtime.set_buffer_remote_id(state, &buffer.id, &remote);
    peers::file_in_rail(state, account_id, &buffer, peer, 1000);
    let now = chrono::Utc::now().timestamp();
    state.runtime.set_buffer_read_only(state, &buffer.id, peers::read_only_for(peer, now));
    // A topic that arrived with no name: the group says what its topics are called.
    if title.is_none() && topic != GENERAL {
        if let Some(handle) = state.runtime.telegram_handle(account_id) {
            let (state, account_id, peer) = (state.clone(), account_id.to_string(), peer.clone());
            tokio::spawn(async move {
                if let Err(e) = sync(&state, &account_id, &handle, &peer).await {
                    tracing::debug!("telegram: reading a group's topics: {e:#}");
                }
            });
        }
    }
    state.runtime.get_buffer(&buffer.id).unwrap_or(buffer)
}

/// Where a message goes, for a message in any conversation: its topic's, in a group with topics.
pub fn buffer_of_message(state: &AppState, account_id: &str, peer: &Peer, raw: &tl::enums::Message) -> Buffer {
    if is_forum(peer) {
        // A topic made, named in the message that makes it.
        let title = match raw {
            tl::enums::Message::Service(s) => match &s.action {
                tl::enums::MessageAction::TopicCreate(a) if !a.title_missing => Some(a.title.clone()),
                _ => None,
            },
            _ => None,
        };
        buffer_for_topic(state, account_id, peer, topic_of(raw), title.as_deref())
    } else {
        peers::buffer_for(state, account_id, peer)
    }
}

/// The conversation that holds a message, given only its group's `remote_id`: the group's own, or - in a group with
/// topics - the topic it was said in.
pub fn holding(state: &AppState, account_id: &str, remote: &str, msg_id: i32) -> Option<Buffer> {
    let mine: Vec<Buffer> = state.runtime.list_buffers().into_iter().filter(|b| b.account_id == account_id).collect();
    if let Some(exact) = mine.iter().find(|b| b.remote_id.as_deref() == Some(remote)) {
        return Some(exact.clone());
    }
    let prefix = format!("{remote}:");
    let topics: Vec<&Buffer> = mine.iter().filter(|b| b.remote_id.as_deref().is_some_and(|r| r.starts_with(&prefix))).collect();
    topics.iter().find(|b| state.store.get_message(&b.id, &msg_id.to_string()).ok().flatten().is_some()).or(topics.first()).map(|b| (*b).clone())
}

/// Reads the group's topics and makes a conversation of each, named as the group names it. The one conversation the
/// group had before it had topics is removed: its messages are in the topics now.
pub async fn sync(state: &AppState, account_id: &str, handle: &Handle, peer: &Peer) -> Result<usize> {
    use grammers_session::Session;
    let group_remote = peers::remote_id(peer.id());
    let peer_ref = handle.session.peer_ref(peer.id()).await.ok().flatten().ok_or_else(|| anyhow!("Telegram has not told this account about that group yet"))?;
    let answer = handle
        .client
        .invoke(&tl::functions::messages::GetForumTopics { peer: peer_ref.into(), q: None, offset_date: 0, offset_id: 0, offset_topic: 0, limit: 100 })
        .await
        .map_err(|e| anyhow!("{}", super::send::explain(&e)))?;
    let tl::enums::messages::ForumTopics::Topics(found) = answer;
    // Gone first, so the topics are not named around it.
    for old in state.runtime.list_buffers() {
        if old.account_id == account_id && old.remote_id.as_deref() == Some(group_remote.as_str()) {
            state.runtime.remove_buffer(state, &old.id);
        }
    }
    let mut count = 0;
    for topic in &found.topics {
        let tl::enums::ForumTopic::Topic(topic) = topic else { continue };
        let buffer = buffer_for_topic(state, account_id, peer, topic.id, Some(&topic.title));
        let now = chrono::Utc::now().timestamp();
        let reason = if topic.closed { Some("This topic is closed.") } else { peers::read_only_for(peer, now) };
        state.runtime.set_buffer_read_only(state, &buffer.id, reason);
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(forum_topic: bool, msg: i32, top: Option<i32>) -> tl::enums::MessageReplyHeader {
        tl::enums::MessageReplyHeader::Header(tl::types::MessageReplyHeader {
            reply_to_scheduled: false,
            forum_topic,
            quote: false,
            reply_to_ephemeral: false,
            reply_to_msg_id: Some(msg),
            reply_to_peer_id: None,
            reply_from: None,
            reply_media: None,
            reply_to_top_id: top,
            quote_text: None,
            quote_entities: None,
            quote_offset: None,
            todo_item_id: None,
            poll_option: None,
        })
    }

    #[test]
    fn a_message_belongs_to_the_topic_it_answers_or_names() {
        assert_eq!(header_topic(&header(true, 45, None)), Some(45), "answering the topic's first message is being in it");
        assert_eq!(header_topic(&header(true, 99, Some(45))), Some(45), "answering a message names the topic too");
        assert_eq!(header_topic(&header(false, 99, None)), None, "an ordinary reply says nothing of topics");
    }

    #[test]
    fn a_topics_conversation_is_named_by_its_group_and_found_by_its_remote_id() {
        assert_eq!(remote_of("-1001234", 45), "-1001234:45");
        assert_eq!(display_name("Chasm", " Support "), "Chasm/#Support");
        assert_eq!(peers::peer_id_of("-1001234:45"), peers::peer_id_of("-1001234"));
    }
}
