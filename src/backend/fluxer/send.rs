//! Saying things: send (with files), edit, delete, react, typing, and telling Fluxer a conversation was read.

use super::messages;
use super::world::{Kind, SEND_MESSAGES};
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use reqwest::Method;
use serde_json::{json, Value};

fn handle_for(state: &AppState, buffer: &Buffer) -> Result<(Handle, String), String> {
    let handle = state.runtime.fluxer_handle(&buffer.account_id).ok_or_else(|| "that Fluxer account is not connected".to_string())?;
    let channel = buffer.remote_id.clone().ok_or_else(|| "this is not a Fluxer conversation".to_string())?;
    Ok((handle, channel))
}

/// What a message to send must be before it is sent: something to say, and no more than the limit.
pub fn check(body: &str, limit: usize) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("There is nothing to send".to_string());
    }
    let length = body.chars().count();
    if length > limit {
        return Err(format!("That message is too long: Fluxer takes at most {limit} characters here, and this has {length}"));
    }
    Ok(())
}

/// Refuses a send while a slowmode Fluxer told of is still running.
fn slowed(handle: &Handle, channel: &str) -> Result<(), String> {
    let until = handle.slow_until(channel);
    let left = until - chrono::Utc::now().timestamp();
    if left > 0 {
        return Err(format!("This channel has slowmode on: wait {left} more seconds before sending again"));
    }
    Ok(())
}

/// What a refused send says, and, if it was a slowmode, when the next one may be made.
fn noted(handle: &Handle, channel: &str, e: super::api::ApiError) -> String {
    if let super::api::ApiError::Refused { code, detail, .. } = &e {
        if code == "SLOWMODE_RATE_LIMITED" {
            let wait = detail["retry_after"].as_f64().unwrap_or(1.0).ceil().max(1.0) as i64;
            handle.set_slow_until(channel, chrono::Utc::now().timestamp() + wait);
        }
    }
    e.to_string()
}

/// What a file is called when it is sent: a spoiler is a name Fluxer reads as one, as Discord does.
pub fn sent_name(path: &str, spoiler: bool) -> String {
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".to_string());
    if spoiler && !name.starts_with("SPOILER_") {
        format!("SPOILER_{name}")
    } else {
        name
    }
}

/// The message itself: the words with custom emoji named by id, a nonce, and the message answered.
fn payload(handle: &Handle, channel: &str, body: &str, reply_to: Option<&str>) -> Value {
    let wire = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let guild = world.channels.get(channel).and_then(|c| c.guild.clone());
        messages::outgoing(&world, guild.as_deref(), body)
    };
    let mut payload = json!({ "content": wire, "nonce": crate::model::next_message_id().to_string().chars().filter(char::is_ascii_alphanumeric).take(32).collect::<String>() });
    if let Some(reply) = reply_to.filter(|r| !r.is_empty()) {
        payload["message_reference"] = json!({ "message_id": reply, "channel_id": channel });
    }
    payload
}

/// Whether the account may write here, by what it knows.
fn may_write(handle: &Handle, channel: &str) -> Result<(), String> {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = world.channels.get(channel) {
        if c.kind != Kind::Notes {
            if let Some(why) = world.read_only(c, chrono::Utc::now().timestamp()) {
                return Err(why.to_string());
            }
            if world.permissions(c) & SEND_MESSAGES == 0 {
                return Err(super::world::NO_SEND_PERMISSION.to_string());
            }
        }
    }
    Ok(())
}

/// Sends a message, in answer to another if `reply_to` names one.
pub async fn send(state: &AppState, buffer: &Buffer, body: &str, reply_to: Option<&str>) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    may_write(&handle, &channel)?;
    check(body, handle.limits.message_length)?;
    slowed(&handle, &channel)?;
    let payload = payload(&handle, &channel, body, reply_to);
    let sent = handle.api.post(&format!("/channels/{channel}/messages"), &payload).await.map_err(|e| noted(&handle, &channel, e))?;
    // At once rather than when the event comes back: the echo finds it kept already.
    messages::record(state, &handle, &sent);
    Ok(())
}

/// Sends files with a message, as one multipart request (`payload_json` and `files[n]`).
pub async fn send_files(state: &AppState, buffer: &Buffer, caption: &str, files: &[(String, bool)], reply_to: Option<&str>, progress: Option<&crate::upload::Progress>) -> Result<(), String> {
    if files.is_empty() {
        return send(state, buffer, caption, reply_to).await;
    }
    let (handle, channel) = handle_for(state, buffer)?;
    may_write(&handle, &channel)?;
    let (most, biggest) = (handle.limits.attachments_per_message, handle.limits.attachment_bytes);
    if files.len() > most {
        return Err(format!("Fluxer takes at most {most} files in a message here, and this has {}", files.len()));
    }
    if !caption.trim().is_empty() {
        check(caption, handle.limits.message_length)?;
    }
    slowed(&handle, &channel)?;
    let total: u64 = files.iter().filter_map(|(p, _)| std::fs::metadata(p).ok().map(|m| m.len())).sum();
    if let Some(p) = progress {
        p.at(crate::upload::Phase::Preparing, total as usize, "Fluxer");
    }
    let mut parts = Vec::new();
    let mut listed = Vec::new();
    for (n, (path, spoiler)) in files.iter().enumerate() {
        let size = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?.len();
        if size > biggest {
            return Err(format!("{} is {} MB and Fluxer takes at most {} MB here", sent_name(path, false), size / 1_048_576, biggest / 1_048_576));
        }
        let bytes = tokio::fs::read(path).await.map_err(|e| format!("reading {path}: {e}"))?;
        let name = sent_name(path, *spoiler);
        listed.push(json!({ "id": n, "filename": name }));
        parts.push((name.clone(), crate::backend::stoat::send::content_type_of(&name).to_string(), bytes));
    }
    let mut payload = payload(&handle, &channel, caption, reply_to);
    payload["attachments"] = json!(listed);
    if let Some(p) = progress {
        p.at(crate::upload::Phase::Sending, total as usize, "Fluxer");
    }
    let sent = handle.api.multipart(Method::POST, &format!("/channels/{channel}/messages"), &payload, &parts).await.map_err(|e| noted(&handle, &channel, e))?;
    messages::record(state, &handle, &sent);
    Ok(())
}

pub async fn edit(state: &AppState, buffer: &Buffer, message_id: &str, body: &str) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    check(body, handle.limits.message_length)?;
    let wire = payload(&handle, &channel, body, None)["content"].clone();
    let edited = handle.api.patch(&format!("/channels/{channel}/messages/{message_id}"), &json!({ "content": wire })).await.map_err(|e| e.to_string())?;
    let text = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let guild = world.channels.get(&channel).and_then(|c| c.guild.clone());
        messages::words(&world, Some(&handle), guild.as_deref(), edited["content"].as_str().unwrap_or(body))
    };
    state.runtime.update_message_body_only(state, &buffer.id, message_id, &text);
    Ok(())
}

pub async fn delete(state: &AppState, buffer: &Buffer, message_id: &str) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    handle.api.delete(&format!("/channels/{channel}/messages/{message_id}")).await.map_err(|e| e.to_string())?;
    state.runtime.delete_message(state, &buffer.id, message_id);
    Ok(())
}

/// Says the account is typing here (Fluxer shows it for about ten seconds).
pub async fn typing(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    handle.api.post(&format!("/channels/{channel}/typing"), &Value::Null).await.map(|_| ()).map_err(|e| e.to_string())
}

/// Says everything in the conversation up to its newest message was read, so it is read on the account's other apps.
pub async fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    let newest = state.store.newest_message_id(&buffer.id).ok().flatten().filter(|id| id.parse::<u64>().is_ok());
    state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
    let Some(newest) = newest else { return Ok(()) };
    let already = handle.world.lock().unwrap_or_else(|e| e.into_inner()).read.get(&channel).and_then(|r| r.0.clone());
    if already.as_deref() == Some(newest.as_str()) {
        return Ok(());
    }
    handle.api.post(&format!("/channels/{channel}/messages/{newest}/ack"), &json!({})).await.map_err(|e| e.to_string())?;
    handle.world.lock().unwrap_or_else(|e| e.into_inner()).read.insert(channel, (Some(newest), 0));
    Ok(())
}

/// A reaction put on a message, or taken off.
pub async fn react(state: &AppState, buffer: &Buffer, message_id: &str, emoji: &str, add: bool) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    let wire = messages::reaction_wire(&handle.world.lock().unwrap_or_else(|e| e.into_inner()), emoji);
    let path = format!("/channels/{channel}/messages/{message_id}/reactions/{}/@me", url::form_urlencoded::byte_serialize(wire.as_bytes()).collect::<String>());
    if add {
        handle.api.put(&path, None).await.map_err(|e| e.to_string())?;
    } else {
        handle.api.delete(&path).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Who put a reaction on a message, by name (the first ten).
pub async fn reactors(handle: &Handle, buffer: &Buffer, message_id: &str, emoji: &str) -> Result<Value, String> {
    let channel = buffer.remote_id.clone().ok_or_else(|| "this is not a Fluxer conversation".to_string())?;
    let wire = messages::reaction_wire(&handle.world.lock().unwrap_or_else(|e| e.into_inner()), emoji);
    let path = format!("/channels/{channel}/messages/{message_id}/reactions/{}", url::form_urlencoded::byte_serialize(wire.as_bytes()).collect::<String>());
    let answer = handle.api.get(&path, &[("limit", "10".to_string())]).await.map_err(|e| e.to_string())?;
    let list = answer.as_array().cloned().or_else(|| answer["users"].as_array().cloned()).unwrap_or_default();
    let guild = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(&channel).and_then(|c| c.guild.clone());
    let users: Vec<Value> = list
        .iter()
        .filter_map(|u| {
            let id = u["id"].as_str()?;
            let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            let name = if world.users.contains_key(id) { world.name_in(guild.as_deref(), id) } else { u["global_name"].as_str().or(u["username"].as_str()).unwrap_or("someone").to_string() };
            Some(json!({ "id": id, "name": name }))
        })
        .collect();
    Ok(json!({ "users": users }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_checked_before_it_goes() {
        assert!(check("  ", 2000).unwrap_err().contains("nothing"));
        assert!(check(&"x".repeat(2001), 2000).unwrap_err().contains("at most 2000"));
        assert!(check("hello", 2000).is_ok());
    }

    #[test]
    fn a_spoiler_is_named_as_one() {
        assert_eq!(sent_name("/tmp/cat.png", true), "SPOILER_cat.png");
        assert_eq!(sent_name("/tmp/cat.png", false), "cat.png");
        assert_eq!(sent_name("/tmp/SPOILER_cat.png", true), "SPOILER_cat.png");
    }
}
