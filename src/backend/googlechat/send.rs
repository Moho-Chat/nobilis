//! What the account says and does: sending messages (with formatting, mentions, quote-replies, threads, and files),
//! editing, deleting, reacting, typing, marking read, muting, and leaving/hiding conversations.

use super::{format, media, messages, pb, Group, Handle};
use crate::model::Buffer;
use crate::notify::NotifyLevel;
use crate::state::AppState;
use rand::Rng;
use std::path::Path;

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

fn message_id_pb(group: &Group, topic_id: &str, msg_id: &str) -> pb::MessageId {
    pb::MessageId {
        parent_id: Some(pb::MessageParentId {
            parent: Some(pb::message_parent_id::Parent::TopicId(pb::TopicId {
                group_id: Some(group.to_pb()),
                topic_id: Some(topic_id.to_string()),
            })),
        }),
        message_id: Some(msg_id.to_string()),
    }
}

fn build_reply_target(handle: &Handle, group: &Group, reply_to: Option<&str>, thread_topic: Option<&str>) -> Option<pb::SendReplyTarget> {
    let reply_id = reply_to.filter(|s| !s.is_empty())?;
    let meta = handle.world().messages.get(reply_id).cloned()?;
    // A SendReplyTarget without create_time gets the whole send rejected by Google Chat.
    if meta.create_time_micros <= 0 {
        return None;
    }
    let topic = thread_topic
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| if meta.topic_id.is_empty() { reply_id.to_string() } else { meta.topic_id });
    Some(pb::SendReplyTarget {
        id: Some(message_id_pb(group, &topic, reply_id)),
        create_time: Some(meta.create_time_micros),
    })
}

fn mime_from_path(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Sends a message (with optional Markdown formatting, `@mentions`, quote-reply, thread, and/or file attachments).
pub async fn send_message(
    state: &AppState,
    buffer: &Buffer,
    body: &str,
    thread: Option<&str>,
    reply_to: Option<&str>,
    files: &[String],
) -> Result<(), String> {
    let (handle, group) = handle_for(state, buffer)?;
    let body = body.trim_end();
    if body.is_empty() && files.is_empty() {
        return Ok(());
    }

    let inner_id = match &group {
        Group::Dm(id) | Group::Space(id) => id.clone(),
    };

    let mut file_annotations: Vec<pb::Annotation> = Vec::new();
    for file_path in files {
        let path = Path::new(file_path);
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let bytes = tokio::fs::read(path).await.map_err(|e| format!("reading attachment {filename}: {e}"))?;
        let mime = mime_from_path(path);
        let meta = media::upload_file(&handle.api.session, &inner_id, &bytes, filename, mime)
            .await
            .map_err(|e| format!("uploading {filename} to Google Chat: {e}"))?;
        file_annotations.push(pb::Annotation {
            r#type: Some(pb::AnnotationType::UploadMetadata as i32),
            chip_render_type: Some(pb::annotation::ChipRenderType::Render as i32),
            metadata: Some(pb::annotation::Metadata::UploadMetadata(meta)),
            ..Default::default()
        });
    }

    let members = handle.world().mentionable_members(&group);
    let (text, text_annotations) = if body.is_empty() {
        (String::new(), Vec::new())
    } else {
        format::parse_outbound(body, &members)
    };
    let mut annotations = file_annotations;
    annotations.extend(text_annotations);

    let thread_topic = thread.filter(|t| !t.is_empty()).map(|t| handle.world().topic_of(t));
    let quote = build_reply_target(&handle, &group, reply_to, thread_topic.as_deref());
    let info = pb::MessageInfo { accept_format_annotations: Some(true), reply_to: quote };

    let sent = match thread_topic {
        Some(topic) => {
            let request = pb::CreateMessageRequest {
                parent_id: Some(pb::MessageParentId {
                    parent: Some(pb::message_parent_id::Parent::TopicId(pb::TopicId {
                        group_id: Some(group.to_pb()),
                        topic_id: Some(topic),
                    })),
                }),
                text_body: Some(text),
                annotations,
                local_id: Some(local_id()),
                message_info: Some(info),
                ..Default::default()
            };
            handle.api.create_message(request).await.map(|r| r.message)
        }
        None => {
            let request = pb::CreateTopicRequest {
                group_id: Some(group.to_pb()),
                text_body: Some(text),
                annotations,
                local_id: Some(local_id()),
                history_v2: Some(true),
                message_info: Some(info),
                ..Default::default()
            };
            handle.api.create_topic(request).await.map(|r| r.topic.and_then(|t| t.replies.into_iter().next()))
        }
    }
    .map_err(|e| e.to_string())?;

    if let Some(message) = sent {
        messages::record(state, &handle, &group, &message).await;
    }
    Ok(())
}

/// Edits a sent message's text and formatting annotations.
pub async fn edit(state: &AppState, buffer: &Buffer, msg_id: &str, body: &str) -> Result<(), String> {
    let (handle, group) = handle_for(state, buffer)?;
    let topic_id = handle.world().topic_of(msg_id);
    let members = handle.world().mentionable_members(&group);
    let (text, annotations) = format::parse_outbound(body.trim_end(), &members);

    let request = pb::EditMessageRequest {
        message_id: Some(message_id_pb(&group, &topic_id, msg_id)),
        text_body: Some(text),
        annotations,
        message_info: Some(pb::MessageInfo { accept_format_annotations: Some(true), ..Default::default() }),
        ..Default::default()
    };
    let resp = handle.api.edit_message(request).await.map_err(|e| e.to_string())?;
    if let Some(message) = resp.message {
        messages::update_live(state, &handle, &group, &message).await;
    } else {
        state.runtime.update_message(state, &buffer.id, msg_id, body.trim_end(), &[], &[]);
    }
    Ok(())
}

/// Deletes a message.
pub async fn delete(state: &AppState, buffer: &Buffer, msg_id: &str) -> Result<(), String> {
    let (handle, group) = handle_for(state, buffer)?;
    let topic_id = handle.world().topic_of(msg_id);
    handle.api.delete_message(message_id_pb(&group, &topic_id, msg_id)).await.map_err(|e| e.to_string())?;
    state.runtime.delete_message(state, &buffer.id, msg_id);
    Ok(())
}

/// Adds or removes an emoji reaction. Strips U+FE0F variation selector, which Google Chat does not use.
pub async fn react(state: &AppState, buffer: &Buffer, msg_id: &str, emoji: &str, add: bool) -> Result<(), String> {
    let (handle, group) = handle_for(state, buffer)?;
    let topic_id = handle.world().topic_of(msg_id);
    let bare = emoji.replace('\u{fe0f}', "");
    handle
        .api
        .update_reaction(message_id_pb(&group, &topic_id, msg_id), &bare, add)
        .await
        .map_err(|e| e.to_string())?;
    state.runtime.update_reaction(state, &buffer.id, msg_id, &bare, true, add);
    Ok(())
}

/// Sends a typing indicator (`TYPING` or `STOPPED`).
pub async fn typing(state: &AppState, buffer: &Buffer, thread: Option<&str>, is_typing: bool) -> Result<(), String> {
    let (handle, group) = handle_for(state, buffer)?;
    let topic = thread.filter(|t| !t.is_empty()).map(|t| handle.world().topic_of(t));
    handle
        .api
        .set_typing_state(&group, topic.as_deref(), is_typing)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Tells Google the conversation has been read, so the web app and the phone stop showing it unread.
pub async fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, group) = handle_for(state, buffer)?;
    handle.api.mark_read(&group, super::now_micros()).await.map(|_| ()).map_err(|e| e.to_string())
}

/// Mutes or unmutes a conversation on Google Chat (`Default` or `Mute`).
pub async fn set_level(state: &AppState, buffer: &Buffer, level: NotifyLevel) -> Result<(), String> {
    if !matches!(level, NotifyLevel::Default | NotifyLevel::Mute) {
        return Err("Google Chat keeps only whether a conversation is muted, so there is nothing between default and mute".to_string());
    }
    let (handle, group) = handle_for(state, buffer)?;
    let muted = level == NotifyLevel::Mute;
    handle.api.update_group_notification_settings(&group, muted).await.map_err(|e| e.to_string())?;
    if let Some(conv) = handle.world().conversations.get_mut(&group) {
        conv.muted = muted;
    }
    state.runtime.set_notify_level(state, &buffer.id, level, level);
    Ok(())
}

/// Leaves a Space (self-removal) or hides a DM in Google Chat, then removes its buffer.
pub async fn leave(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, group) = handle_for(state, buffer)?;
    match &group {
        Group::Dm(_) => {
            handle.api.hide_group(&group, true).await.map_err(|e| e.to_string())?;
        }
        Group::Space(_) => {
            let own_id = handle.world().own_id.clone();
            handle.api.remove_memberships(&group, &own_id).await.map_err(|e| e.to_string())?;
        }
    }
    {
        let mut world = handle.world();
        world.conversations.remove(&group);
        world.titles.remove(&group);
    }
    Ok(())
}
