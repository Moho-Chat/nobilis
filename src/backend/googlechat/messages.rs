//! Messages into the store: live ones from the event stream, live edits, conversation history, and thread replies.
//!
//! A conversation is a list of topics, and a topic a list of messages: in a direct message or a flat space each
//! message is a topic of its own (its id the topic's), while a threaded space's topic is a thread - its first message
//! the head, the rest replies to it.

use super::{format, pb, world, Group, Handle};
use crate::model::{Message, ReplyPreview};
use crate::state::AppState;

/// How many topics a conversation's history is read in at first, and how many more each time it is read further back.
const HISTORY_STEP: i32 = 40;
/// The furthest back history is read: `list_topics` has no cursor, so going back means asking for more of them.
const HISTORY_MAX: i32 = 1000;

fn topic_of_message(message: &pb::Message) -> String {
    message
        .id
        .as_ref()
        .and_then(|m| m.parent_id.as_ref())
        .and_then(|p| p.parent.as_ref())
        .map(|pb::message_parent_id::Parent::TopicId(t)| t.topic_id().to_string())
        .unwrap_or_default()
}

/// Stores one message. A message that is not its topic's head is shown as a reply in that thread.
pub async fn record(state: &AppState, handle: &Handle, group: &Group, message: &pb::Message) -> bool {
    let Some(id) = message.id.as_ref().map(|m| m.message_id().to_string()).filter(|s| !s.is_empty()) else { return false };
    if message.delete_time() > 0 {
        return false;
    }
    // Sent with history off and already past its day: the server has deleted it, and so does moho.
    if super::retention::message_expiry(message).is_some_and(|at| at <= super::retention::now_secs()) {
        if let Some(buffer) = world::buffer_of(state, &handle.account_id, group) {
            state.runtime.delete_message(state, &buffer.id, &id);
        }
        return false;
    }
    let topic = topic_of_message(message);
    let author = message.creator.as_ref().and_then(|c| c.user_id.as_ref()).map(|u| u.id().to_string()).unwrap_or_default();
    // Remember topic_id and create_time_micros so edits, deletes, reactions and quote-replies can reference this message.
    let known = {
        let mut world = handle.world();
        world.remember_message(&id, &topic, message.create_time());
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
    let mut rendered = format::render(message, &own_id, &handle.account_id);
    // A system message's words first: Google's own text for some is nothing at all (history turned off or on).
    if message.message_type() == pb::message::MessageType::SystemMessage {
        let world = handle.world();
        if let Some(words) = system_words(message, |id| world.name_of(id), &from) {
            rendered.body = words;
        }
    }
    if rendered.body.trim().is_empty() && rendered.attachments.is_empty() && rendered.embeds.is_empty() {
        return false;
    }
    let reply = if !topic.is_empty() && topic != id {
        Some(ReplyPreview { id: topic.clone(), from: String::new(), body: String::new(), thread: true, forwarded: false, thread_root: Some(topic), chat: None })
    } else {
        message.reply_to.as_ref().and_then(|r| r.id.as_ref()).map(|r| r.message_id().to_string()).filter(|s| !s.is_empty()).map(|quoted| {
            let quoted_from = message.reply_to.as_ref().and_then(|r| r.sender.as_ref().or(r.sender_again.as_ref())).map(|u| u.name().to_string()).unwrap_or_default();
            let quoted_body = message.reply_to.as_ref().map(|r| r.text_body().to_string()).unwrap_or_default();
            ReplyPreview { id: quoted, from: quoted_from, body: quoted_body, thread: false, forwarded: false, thread_root: None, chat: None }
        })
    };
    let is_system = message.message_type() == pb::message::MessageType::SystemMessage;
    let kind = if is_system { "system" } else { "chat" };
    let shown_from = if is_system { "" } else { from.as_str() };
    let highlight = !is_system && author != own_id && rendered.mentions_own;
    let sent_at = super::micros_to_secs(message.create_time());
    let stored = state.runtime.record_message_at(
        state,
        &handle.account_id,
        &buffer.name,
        &buffer.kind,
        shown_from,
        &rendered.body,
        false,
        kind,
        reply,
        Some(id.clone()),
        highlight,
        if is_system { None } else { avatar },
        rendered.embeds,
        rendered.attachments,
        (!is_system && !author.is_empty()).then_some(author),
        (sent_at > 0).then_some(sent_at),
        if is_system { None } else { rendered.html },
        None,
    );
    if stored && !rendered.reactions.is_empty() {
        let _ = state.store.set_reactions(&buffer.id, &id, &rendered.reactions);
    }
    // When it goes, if it was sent with history off - noted whether it is new or seen again in history.
    super::retention::note(state, &buffer.id, &id, message);
    // "History turned off/on": the conversation's setting changes with it.
    if is_system {
        if let Some(off) = super::retention::announced_change(message) {
            let current = handle.world().conversations.get(group).map(|c| c.history_off);
            // Only the newest announcement counts: history is read oldest first, so an old one is overtaken in turn.
            if current != Some(off) {
                super::retention::set_history_off(state, handle, group, off);
            }
        }
    }
    stored
}

/// Applies a live `MESSAGE_UPDATED` edit to an existing message in the store (or records it if unseen).
pub async fn update_live(state: &AppState, handle: &Handle, group: &Group, message: &pb::Message) {
    let Some(id) = message.id.as_ref().map(|m| m.message_id().to_string()).filter(|s| !s.is_empty()) else { return };
    if message.delete_time() > 0 {
        if let Some(buffer) = world::buffer_of(state, &handle.account_id, group) {
            state.runtime.delete_message(state, &buffer.id, &id);
        }
        return;
    }
    let Some(buffer) = world::buffer_of(state, &handle.account_id, group) else {
        record(state, handle, group, message).await;
        return;
    };
    let own_id = handle.world().own_id.clone();
    let rendered = format::render(message, &own_id, &handle.account_id);
    let _ = state.store.set_message_html(&buffer.id, &id, rendered.html.as_deref());
    if !state.runtime.update_message(state, &buffer.id, &id, &rendered.body, &rendered.embeds, &rendered.attachments) {
        record(state, handle, group, message).await;
    } else if let Some(html) = rendered.html {
        state.events.emit("messageUpdated", serde_json::json!({ "bufferId": buffer.id, "id": id, "body": rendered.body, "html": html, "edited": true }));
    }
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
    // The store keeps whole seconds and orders a second's messages as they were stored. Reading further back stores
    // older messages after newer ones, so where a page boundary falls inside one second the older would land below
    // the newer (seen live: 16 above 15). The messages of that second already held are taken back and stored again
    // with the rest, in order.
    if further {
        if let Ok(Some(boundary)) = state.store.oldest_ts(&buffer.id) {
            let older_in_boundary = messages.iter().any(|m| super::micros_to_secs(m.create_time()) == boundary && !state.store.get_message(&buffer.id, m.id.as_ref().map(|i| i.message_id()).unwrap_or("")).ok().flatten().is_some());
            if older_in_boundary {
                for m in messages.iter().filter(|m| super::micros_to_secs(m.create_time()) == boundary) {
                    if let Some(id) = m.id.as_ref().map(|i| i.message_id()).filter(|s| !s.is_empty()) {
                        state.runtime.delete_message(state, &buffer.id, id);
                    }
                }
            }
        }
    }
    let mut count = 0;
    for message in &messages {
        if record(state, handle, &group, message).await {
            count += 1;
        }
    }
    Ok(count)
}

/// Fetches all replies in a thread (topic) via `list_messages` and returns them from the store.
pub async fn fetch_thread(state: &AppState, handle: &Handle, buffer: &crate::model::Buffer, root_id: &str) -> anyhow::Result<Vec<Message>> {
    let Some(group) = buffer.remote_id.as_deref().and_then(Group::from_remote_id) else {
        return Ok(Vec::new());
    };
    let topic_id = handle.world().topic_of(root_id);
    if let Ok(resp) = handle.api.list_messages(&group, &topic_id, 100).await {
        let mut msgs = resp.messages;
        msgs.sort_by_key(|m| m.create_time());
        for m in &msgs {
            record(state, handle, &group, m).await;
        }
    }
    Ok(state.store.thread_messages(&buffer.id, &topic_id).unwrap_or_default())
}

/// What a system message says, the way Google Chat's web app says it, from its annotation - Google's own text for these
/// is a bare "Space Updated". `None` for what is not one of these, which keeps Google's text.
pub fn system_words(message: &pb::Message, name_of: impl Fn(&str) -> String, author: &str) -> Option<String> {
    let who = |id: Option<&str>| id.filter(|s| !s.is_empty()).map(&name_of).filter(|s| !s.is_empty()).unwrap_or_else(|| author.to_string());
    message.annotations.iter().find_map(|a| match &a.metadata {
        Some(pb::annotation::Metadata::GroupRetentionSettingsUpdated(m)) => {
            let by = who(m.initiator.as_ref().map(|u| u.id()));
            Some(if super::retention::is_ephemeral(m.retention_settings.as_ref()) {
                format!("{by} turned off history: new messages are deleted after 24 hours")
            } else {
                format!("{by} turned on history")
            })
        }
        Some(pb::annotation::Metadata::RoomUpdated(m)) => {
            let by = m
                .initiator
                .as_ref()
                .map(|u| if u.name().is_empty() { who(u.user_id.as_ref().map(|i| i.id())) } else { u.name().to_string() })
                .unwrap_or_else(|| who(None));
            if let Some(r) = m.rename_metadata.as_ref().filter(|r| !r.new_name().is_empty()) {
                return Some(if r.prev_name().is_empty() {
                    format!("{by} renamed the space to \u{201c}{}\u{201d}", r.new_name())
                } else {
                    format!("{by} renamed the space from \u{201c}{}\u{201d} to \u{201c}{}\u{201d}", r.prev_name(), r.new_name())
                });
            }
            let d = m.group_details_metadata.as_ref()?;
            let (new, old) = (d.new_group_details.clone().unwrap_or_default(), d.prev_group_details.clone().unwrap_or_default());
            let what = match (new.description() != old.description(), new.guidelines() != old.guidelines()) {
                (true, true) => "description and guidelines",
                (false, true) => "guidelines",
                _ => "description",
            };
            Some(format!("{by} updated the space {what}"))
        }
        _ => None,
    })
}

#[cfg(test)]
mod system_words_tests {
    use super::*;

    fn with(metadata: pb::annotation::Metadata) -> pb::Message {
        pb::Message { annotations: vec![pb::Annotation { metadata: Some(metadata), ..Default::default() }], ..Default::default() }
    }

    #[test]
    fn says_what_changed_and_who_did_it() {
        let names = |id: &str| if id == "1" { "John".to_string() } else { String::new() };
        let off = with(pb::annotation::Metadata::GroupRetentionSettingsUpdated(pb::GroupRetentionSettingsUpdatedMetaData {
            retention_settings: Some(pb::RetentionSettings { state: Some(pb::retention_settings::RetentionState::EphemeralOneDay as i32), expiry_timestamp: None }),
            initiator: Some(super::super::user_id("1")),
        }));
        assert_eq!(system_words(&off, names, "x").as_deref(), Some("John turned off history: new messages are deleted after 24 hours"));
        let details = with(pb::annotation::Metadata::RoomUpdated(pb::RoomUpdatedMetadata {
            group_details_metadata: Some(pb::room_updated_metadata::GroupDetailsUpdatedMetadata {
                new_group_details: Some(pb::GroupDetails { description: Some("a".into()), guidelines: Some("new".into()) }),
                prev_group_details: Some(pb::GroupDetails { description: Some("a".into()), guidelines: Some("old".into()) }),
            }),
            ..Default::default()
        }));
        assert_eq!(system_words(&details, names, "Ada").as_deref(), Some("Ada updated the space guidelines"));
        let rename = with(pb::annotation::Metadata::RoomUpdated(pb::RoomUpdatedMetadata {
            rename_metadata: Some(pb::room_updated_metadata::RoomRenameMetadata { new_name: Some("B".into()), prev_name: Some("A".into()) }),
            ..Default::default()
        }));
        assert_eq!(system_words(&rename, names, "Ada").as_deref(), Some("Ada renamed the space from \u{201c}A\u{201d} to \u{201c}B\u{201d}"));
        assert_eq!(system_words(&pb::Message::default(), names, "Ada"), None);
    }
}
