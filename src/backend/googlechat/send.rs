//! What the account says: messages, and that a conversation has been read.

use super::{messages, pb, Group, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use rand::Rng;

/// The id a message is sent with, echoed back on it when it arrives through the event stream.
fn local_id() -> String {
    let n: u64 = rand::thread_rng().gen();
    format!("{n:016x}")
}

fn handle_for(state: &AppState, buffer: &Buffer) -> Result<(Handle, Group), String> {
    let handle = state.runtime.googlechat_handle(&buffer.account_id).ok_or("that Google Chat account is not connected")?;
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("this is not a Google Chat conversation")?;
    Ok((handle, group))
}

/// Sends text to a conversation: a message of its own, or - with `thread` - a reply in that thread (topic).
pub async fn send_text(state: &AppState, buffer: &Buffer, body: &str, thread: Option<&str>) -> Result<(), String> {
    let (handle, group) = handle_for(state, buffer)?;
    let body = body.trim_end();
    if body.is_empty() {
        return Ok(());
    }
    let info = pb::MessageInfo { accept_format_annotations: Some(true), ..Default::default() };
    let sent = match thread.filter(|t| !t.is_empty()) {
        Some(topic) => {
            let request = pb::CreateMessageRequest {
                parent_id: Some(pb::MessageParentId { parent: Some(pb::message_parent_id::Parent::TopicId(pb::TopicId { group_id: Some(group.to_pb()), topic_id: Some(topic.to_string()) })) }),
                text_body: Some(body.to_string()),
                local_id: Some(local_id()),
                message_info: Some(info),
                ..Default::default()
            };
            handle.api.create_message(request).await.map(|r| r.message)
        }
        None => {
            let request = pb::CreateTopicRequest {
                group_id: Some(group.to_pb()),
                text_body: Some(body.to_string()),
                local_id: Some(local_id()),
                history_v2: Some(true),
                message_info: Some(info),
                ..Default::default()
            };
            handle.api.create_topic(request).await.map(|r| r.topic.and_then(|t| t.replies.into_iter().next()))
        }
    }
    .map_err(|e| e.to_string())?;
    // The message as Google stored it, so it shows at once with its real id; the echo from the stream is then the
    // same message and is not stored twice.
    if let Some(message) = sent {
        messages::record(state, &handle, &group, &message).await;
    }
    Ok(())
}

/// Tells Google the conversation has been read, so the web app and the phone stop showing it unread.
pub async fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, group) = handle_for(state, buffer)?;
    handle.api.mark_read(&group, super::now_micros()).await.map(|_| ()).map_err(|e| e.to_string())
}
