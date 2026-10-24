//! Saying things: send, edit, delete, typing, and telling Stoat a conversation was read.

use super::messages;
use super::world::{Kind, SEND_MESSAGE};
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use serde_json::json;

fn handle_for(state: &AppState, buffer: &Buffer) -> Result<(Handle, String), String> {
    let handle = state.runtime.stoat_handle(&buffer.account_id).ok_or_else(|| "that Stoat account is not connected".to_string())?;
    let channel = buffer.remote_id.clone().ok_or_else(|| "this is not a Stoat conversation".to_string())?;
    Ok((handle, channel))
}

/// How long a message may be for this account: a new one is held to a lower limit for its first hours, which the
/// instance says, and the account's own id says how old the account is.
pub fn message_limit(handle: &Handle, now_ms: i64) -> usize {
    let own = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone();
    let made = messages::ulid_ms(&own).unwrap_or(0);
    let new = made > 0 && now_ms - made < (handle.instance.new_user_hours as i64) * 3_600_000;
    if new {
        handle.instance.new_message_length
    } else {
        handle.instance.message_length
    }
}

/// What a message to send must be before it is sent: something to say, and no more than the limit.
pub fn check(body: &str, limit: usize) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("There is nothing to send".to_string());
    }
    let length = body.chars().count();
    if length > limit {
        return Err(format!("That message is too long: Stoat takes at most {limit} characters here, and this has {length}"));
    }
    Ok(())
}

/// Refuses a send while a slowmode Stoat told of is still running.
fn slowed(handle: &Handle, channel: &str) -> Result<(), String> {
    let until = handle.world.lock().unwrap_or_else(|e| e.into_inner()).slow_until.get(channel).copied().unwrap_or(0);
    let left = until - chrono::Utc::now().timestamp();
    if left > 0 {
        return Err(format!("This channel has slowmode on: wait {left} more seconds before sending again"));
    }
    Ok(())
}

/// What a refused send says, and, if it was a slowmode, when the next one may be made.
fn noted_slowmode(handle: &Handle, channel: &str, e: super::api::ApiError) -> String {
    if let super::api::ApiError::Refused { kind, detail, .. } = &e {
        if kind == "InSlowmode" {
            let wait = detail["retry_after"].as_i64().unwrap_or(1).max(1);
            handle.world.lock().unwrap_or_else(|e| e.into_inner()).slow_until.insert(channel.to_string(), chrono::Utc::now().timestamp() + wait);
        }
    }
    words(e)
}

fn words(e: super::api::ApiError) -> String {
    e.to_string()
}

/// What a file is called when it is sent: a spoiler is a name Stoat reads as one.
pub fn sent_name(path: &str, spoiler: bool) -> String {
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".to_string());
    if spoiler && !name.starts_with("SPOILER_") {
        format!("SPOILER_{name}")
    } else {
        name
    }
}

/// What a file's kind of content is, from its name, for the file server.
pub fn content_type_of(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "txt" => "text/plain",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// How many files a message may carry and how large each may be, for this account now.
pub fn file_limits(handle: &Handle, now_ms: i64) -> (usize, u64) {
    let own = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone();
    let made = messages::ulid_ms(&own).unwrap_or(0);
    let new = made > 0 && now_ms - made < (handle.instance.new_user_hours as i64) * 3_600_000;
    if new {
        (handle.instance.new_message_attachments, handle.instance.new_attachment_bytes)
    } else {
        (handle.instance.message_attachments, handle.instance.attachment_bytes)
    }
}

/// Sends files with a message: each is put on the file server and named in the message.
pub async fn send_files(state: &AppState, buffer: &Buffer, caption: &str, files: &[(String, bool)], reply_to: Option<&str>, progress: Option<&crate::upload::Progress>) -> Result<(), String> {
    if files.is_empty() {
        return send(state, buffer, caption, reply_to).await;
    }
    let (handle, channel) = handle_for(state, buffer)?;
    let (most, biggest) = file_limits(&handle, chrono::Utc::now().timestamp_millis());
    if files.len() > most {
        return Err(format!("Stoat takes at most {most} files in a message here, and this has {}", files.len()));
    }
    if !caption.trim().is_empty() {
        check(caption, message_limit(&handle, chrono::Utc::now().timestamp_millis()))?;
    }
    let server = handle.instance.autumn.clone().ok_or("this instance has no file server")?;
    let total: u64 = files.iter().filter_map(|(p, _)| std::fs::metadata(p).ok().map(|m| m.len())).sum();
    if let Some(p) = progress {
        p.at(crate::upload::Phase::Preparing, total as usize, "Stoat");
    }
    let mut ids = Vec::new();
    let mut done = 0u64;
    for (path, spoiler) in files {
        let size = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?.len();
        if size > biggest {
            return Err(format!("{} is {} MB and Stoat takes at most {} MB here", sent_name(path, false), size / 1_000_000, biggest / 1_000_000));
        }
        let bytes = tokio::fs::read(path).await.map_err(|e| format!("reading {path}: {e}"))?;
        let name = sent_name(path, *spoiler);
        if let Some(p) = progress {
            p.sent(done, total, "Stoat");
        }
        let id = handle.api.upload(&server, "attachments", &name, content_type_of(&name), bytes).await.map_err(words)?;
        done += size;
        ids.push(id);
    }
    if let Some(p) = progress {
        p.at(crate::upload::Phase::Waiting, total as usize, "Stoat");
    }
    let caption = handle.world.lock().unwrap_or_else(|e| e.into_inner()).pipe_outgoing(caption);
    let mut payload = json!({ "content": caption, "attachments": ids, "nonce": format!("moho-{}", crate::model::next_message_id()) });
    if let Some(reply) = reply_to.filter(|r| !r.is_empty()) {
        payload["replies"] = json!([{ "id": reply, "mention": false }]);
    }
    let sent = handle.api.post(&format!("/channels/{channel}/messages"), &payload).await.map_err(words)?;
    messages::record(state, &handle, &sent);
    Ok(())
}

/// Sends a message, in answer to another if `reply_to` names one.
pub async fn send(state: &AppState, buffer: &Buffer, body: &str, reply_to: Option<&str>) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = world.channels.get(&channel) {
            if world.permissions(c, chrono::Utc::now().timestamp()) & SEND_MESSAGE == 0 && c.kind != Kind::Saved {
                return Err(world.read_only(c, chrono::Utc::now().timestamp()).unwrap_or(super::world::NO_SEND_PERMISSION).to_string());
            }
        }
    }
    check(body, message_limit(&handle, chrono::Utc::now().timestamp_millis()))?;
    slowed(&handle, &channel)?;
    let body = &handle.world.lock().unwrap_or_else(|e| e.into_inner()).pipe_outgoing(body);
    let mut payload = json!({ "content": body, "nonce": format!("moho-{}", crate::model::next_message_id()) });
    if let Some(reply) = reply_to.filter(|r| !r.is_empty()) {
        payload["replies"] = json!([{ "id": reply, "mention": false }]);
    }
    let sent = handle.api.post(&format!("/channels/{channel}/messages"), &payload).await.map_err(|e| noted_slowmode(&handle, &channel, e))?;
    // At once rather than when the event comes back: the echo finds it kept already.
    messages::record(state, &handle, &sent);
    Ok(())
}

pub async fn edit(state: &AppState, buffer: &Buffer, message_id: &str, body: &str) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    check(body, message_limit(&handle, chrono::Utc::now().timestamp_millis()))?;
    let wire = handle.world.lock().unwrap_or_else(|e| e.into_inner()).pipe_outgoing(body);
    handle.api.patch(&format!("/channels/{channel}/messages/{message_id}"), &json!({ "content": wire })).await.map_err(words)?;
    let text = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let server = world.channels.get(&channel).and_then(|c| c.server.clone());
        messages::words(&world, Some((&handle.instance, &handle.account_id)), server.as_deref(), body)
    };
    state.runtime.update_message_body_only(state, &buffer.id, message_id, &text);
    Ok(())
}

pub async fn delete(state: &AppState, buffer: &Buffer, message_id: &str) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    handle.api.delete(&format!("/channels/{channel}/messages/{message_id}")).await.map_err(words)?;
    state.runtime.delete_message(state, &buffer.id, message_id);
    Ok(())
}

/// Says the account is typing here.
pub fn typing(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    handle.socket.send(json!({ "type": "BeginTyping", "channel": channel }).to_string()).map_err(|_| "the connection is not up".to_string())
}

/// Says everything in the conversation up to its newest message was read, so it is read on the account's other apps.
pub async fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    let newest = state.store.newest_message_id(&buffer.id).ok().flatten().filter(|id| id.len() == 26);
    state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
    let Some(newest) = newest else { return Ok(()) };
    handle.api.put(&format!("/channels/{channel}/ack/{newest}"), None).await.map_err(words)?;
    Ok(())
}

/// A reaction put on a message, or taken off.
pub async fn react(state: &AppState, buffer: &Buffer, message_id: &str, emoji: &str, add: bool) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    // A custom emoji is named by its id on the wire, and by its name in what the window shows.
    let wire = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let name = messages::reaction_name(emoji);
        world.emojis.iter().find(|(_, e)| e.name == name).map(|(id, _)| id.clone()).unwrap_or_else(|| emoji.to_string())
    };
    if add {
        let already: Vec<String> = state.store.get_message(&buffer.id, message_id).ok().flatten().map(|m| m.reactions.iter().map(|r| r.emoji.clone()).collect()).unwrap_or_default();
        super::limits::may_react(&handle.instance, &already, emoji)?;
    }
    let path = format!("/channels/{channel}/messages/{message_id}/reactions/{}", urlencode(&wire));
    if add {
        handle.api.put(&path, None).await.map_err(words)?;
    } else {
        handle.api.delete(&path).await.map_err(words)?;
    }
    state.runtime.update_reaction(state, &buffer.id, message_id, emoji, true, add);
    Ok(())
}

fn urlencode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_needs_something_in_it_and_not_more_than_the_limit() {
        assert!(check("hello", 2000).is_ok());
        assert!(check("   ", 2000).unwrap_err().contains("nothing to send"));
        assert!(check(&"a".repeat(2001), 2000).unwrap_err().contains("at most 2000"));
        assert!(check(&"é".repeat(2000), 2000).is_ok(), "a limit of characters, not of bytes");
    }

    #[test]
    fn a_file_is_sent_under_its_own_name_and_a_spoiler_under_the_name_stoat_reads_as_one() {
        assert_eq!(sent_name("/home/me/cat.png", false), "cat.png");
        assert_eq!(sent_name("/home/me/cat.png", true), "SPOILER_cat.png");
        assert_eq!(sent_name("/home/me/SPOILER_cat.png", true), "SPOILER_cat.png", "not twice");
        assert_eq!(content_type_of("a.JPG"), "image/jpeg");
        assert_eq!(content_type_of("a.unknown"), "application/octet-stream");
    }

    #[test]
    fn a_slowmode_stoat_told_of_is_kept_to_and_not_asked_about_again() {
        let h = crate::backend::stoat::tests::bare_handle("stoat:ME");
        assert!(slowed(&h, "C").is_ok());
        let said = noted_slowmode(&h, "C", super::super::api::ApiError::Refused { status: 429, kind: "InSlowmode".into(), detail: json!({ "type": "InSlowmode", "retry_after": 20 }) });
        assert!(said.contains("slowmode") && said.contains("20"), "{said}");
        let wait = slowed(&h, "C").unwrap_err();
        assert!(wait.contains("slowmode on: wait"), "{wait}");
        assert!(slowed(&h, "other").is_ok(), "only that channel");
    }

    #[test]
    fn an_emoji_is_written_in_an_address_the_way_the_server_reads_it() {
        assert_eq!(urlencode("👍"), "%F0%9F%91%8D");
        assert_eq!(urlencode("01EMOJ000000000000000000AA"), "01EMOJ000000000000000000AA");
    }
}
