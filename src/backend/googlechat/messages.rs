//! Messages into the store: live ones from the event stream, and a conversation's history.
//!
//! A conversation is a list of topics, and a topic a list of messages: in a direct message or a flat space each
//! message is a topic of its own (its id the topic's), while a threaded space's topic is a thread - its first message
//! the head, the rest replies to it.

use super::{format, pb, world, Group, Handle};
use crate::model::ReplyPreview;
use crate::state::AppState;

/// How many topics a conversation's history is read in at first, and how many more each time it is read further back.
const HISTORY_STEP: i32 = 40;
/// The furthest back history is read: `list_topics` has no cursor, so going back means asking for more of them.
const HISTORY_MAX: i32 = 1000;

/// Stores one message. `topic` is the topic it is in, when known; a message that is not its topic's head is shown as a
/// reply in that thread.
pub async fn record(state: &AppState, handle: &Handle, group: &Group, message: &pb::Message) -> bool {
    let Some(id) = message.id.as_ref().map(|m| m.message_id().to_string()).filter(|s| !s.is_empty()) else { return false };
    if message.delete_time() > 0 {
        return false;
    }
    let author = message.creator.as_ref().and_then(|c| c.user_id.as_ref()).map(|u| u.id().to_string()).unwrap_or_default();
    // An author the account has not met is asked about first, so the line goes in with a name.
    let known = {
        let mut world = handle.world();
        if let Some(creator) = &message.creator {
            if !creator.name().is_empty() {
                world.learn(creator);
            }
        }
        author.is_empty() || world.people.contains_key(&author)
    };
    if !known {
        world::learn_people(handle, vec![author.clone()]).await;
    }

    let buffer = match world::buffer_of(state, &handle.account_id, group) {
        Some(b) => b,
        None => {
            // A conversation that was not in the list yet: somebody wrote first.
            let unknown = {
                let mut world = handle.world();
                world.conversations.entry(group.clone()).or_default();
                if group.is_dm() {
                    let own = world.own_id.clone();
                    let conversation = world.conversations.get_mut(group).expect("just made");
                    for who in [author.clone(), own] {
                        if !who.is_empty() && !conversation.members.contains(&who) {
                            conversation.members.push(who);
                        }
                    }
                }
                world.unknown_people()
            };
            if !unknown.is_empty() {
                world::learn_people(handle, unknown).await;
            }
            world::register(state, handle, group, false)
        }
    };

    let (from, avatar, own_id) = {
        let world = handle.world();
        (world.name_of(&author), world.people.get(&author).and_then(|p| p.avatar.clone()), world.own_id.clone())
    };
    let rendered = format::render(message, &own_id);
    if rendered.body.trim().is_empty() {
        return false;
    }
    let topic = message.id.as_ref().and_then(|m| m.parent_id.as_ref()).and_then(|p| p.parent.as_ref()).map(|pb::message_parent_id::Parent::TopicId(t)| t.topic_id().to_string()).unwrap_or_default();
    let reply = if !topic.is_empty() && topic != id {
        Some(ReplyPreview { id: topic.clone(), from: String::new(), body: String::new(), thread: true, forwarded: false, thread_root: Some(topic), chat: None })
    } else {
        message.reply_to.as_ref().and_then(|r| r.id.as_ref()).map(|r| r.message_id().to_string()).filter(|s| !s.is_empty()).map(|quoted| {
            let quoted_from = message.reply_to.as_ref().and_then(|r| r.sender.as_ref().or(r.sender_again.as_ref())).map(|u| u.name().to_string()).unwrap_or_default();
            let quoted_body = message.reply_to.as_ref().map(|r| r.text_body().to_string()).unwrap_or_default();
            ReplyPreview { id: quoted, from: quoted_from, body: quoted_body, thread: false, forwarded: false, thread_root: None, chat: None }
        })
    };
    let highlight = author != own_id && rendered.mentions_own;
    let sent_at = super::micros_to_secs(message.create_time());
    state.runtime.record_message_at(
        state,
        &handle.account_id,
        &buffer.name,
        &buffer.kind,
        &from,
        &rendered.body,
        false,
        "chat",
        reply,
        Some(id),
        highlight,
        avatar,
        Vec::new(),
        Vec::new(),
        (!author.is_empty()).then_some(author),
        (sent_at > 0).then_some(sent_at),
        rendered.html,
        None,
    )
}

/// Reads a conversation's newest history into the store; `further` reads further back than the last time.
pub async fn history(state: &AppState, handle: &Handle, buffer: &crate::model::Buffer, further: bool) -> anyhow::Result<usize> {
    let Some(group) = buffer.remote_id.as_deref().and_then(Group::from_remote_id) else { return Ok(0) };
    let depth = {
        let mut world = handle.world();
        let depth = world.history_depth.entry(group.clone()).or_insert(0);
        if *depth == 0 || further {
            *depth = (*depth + HISTORY_STEP).min(HISTORY_MAX);
        }
        *depth
    };
    let response = handle.api.list_topics(&group, depth).await?;
    let mut messages: Vec<pb::Message> = response.topics.into_iter().flat_map(|t| t.replies).collect();
    messages.sort_by_key(|m| m.create_time());
    let mut count = 0;
    for message in &messages {
        if record(state, handle, &group, message).await {
            count += 1;
        }
    }
    Ok(count)
}
