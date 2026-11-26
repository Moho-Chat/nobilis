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

/// Fluxer's attachment flag for a spoiler (`IS_SPOILER`, `http-api/messages.mdx`). Fluxer reads this, not the
/// `SPOILER_` name Discord uses: a file sent under that name alone was shown uncovered (seen on fluxer.com, 2026-11-18).
pub const IS_SPOILER: u64 = 1 << 3;

/// What a file is called when it is sent; with `spoiler`, Discord's `SPOILER_` name as well (kept for the spoiler flag
/// only to be read where the name is, and harmless where the flag is).
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
    if buffer.slowmode_secs > 0 {
        handle.set_slow_until(&channel, chrono::Utc::now().timestamp() + buffer.slowmode_secs as i64);
    }
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
    let mut upload_req = Vec::new();
    for (n, (path, spoiler)) in files.iter().enumerate() {
        let size = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?.len();
        if size > biggest {
            return Err(format!("{} is {} MB and Fluxer takes at most {} MB here", sent_name(path, false), size / 1_048_576, biggest / 1_048_576));
        }
        let bytes = tokio::fs::read(path).await.map_err(|e| format!("reading {path}: {e}"))?;
        let name = sent_name(path, false);
        let ctype = crate::backend::stoat::send::content_type_of(&name).to_string();
        listed.push(json!({ "id": n, "filename": name, "flags": if *spoiler { IS_SPOILER } else { 0 } }));
        upload_req.push(json!({ "id": n, "filename": name, "file_size": size, "content_type": ctype }));
        parts.push((name, ctype, bytes));
    }
    if let Some(p) = progress {
        p.at(crate::upload::Phase::Sending, total as usize, "Fluxer");
    }
    // Use presigned uploads when the instance advertises `presigned_attachment_uploads`, falling back to multipart if
    // the instance turns the feature off at runtime.
    if handle.instance.presigned_uploads {
        match handle.api.post(&format!("/channels/{channel}/attachments"), &json!({ "attachments": upload_req })).await {
            Ok(plan) => {
                let uploads = plan["attachments"].as_array().cloned().unwrap_or_default();
                if uploads.len() == parts.len() {
                    let router = crate::net::route::router();
                    let raw = router.client_if("fluxer", router.routed(&handle.account_id), |b| b.user_agent(super::user_agent()).timeout(std::time::Duration::from_secs(300)));
                    let mut to_complete = Vec::new();
                    let mut preuploaded = Vec::new();
                    let mut failed: Option<String> = None;
                    for (idx, u) in uploads.iter().enumerate() {
                        let (ref name, ref ctype, ref bytes) = parts[idx];
                        let spoiler = files[idx].1;
                        let key = u["upload_filename"].as_str().unwrap_or_default();
                        let mode = u["upload_mode"].as_str().unwrap_or("singlepart");
                        if mode == "multipart" {
                            let part_size = u["part_size"].as_u64().unwrap_or(10 * 1024 * 1024) as usize;
                            let upload_id = u["upload_id"].as_str().unwrap_or_default();
                            for part in u["parts"].as_array().into_iter().flatten() {
                                let num = part["part_number"].as_u64().unwrap_or(1) as usize;
                                let Some(url) = part["upload_url"].as_str() else { continue };
                                let start = (num.saturating_sub(1)) * part_size;
                                let end = (start + part_size).min(bytes.len());
                                if let Err(e) = raw.put(url).body(bytes[start..end].to_vec()).send().await.and_then(|r| r.error_for_status()) {
                                    failed = Some(format!("uploading {name}: {e}"));
                                    break;
                                }
                            }
                            if failed.is_some() {
                                break;
                            }
                            to_complete.push(json!({ "upload_filename": key, "upload_id": upload_id }));
                        } else if let Some(url) = u["upload_url"].as_str() {
                            if let Err(e) = raw.put(url).header("Content-Type", ctype.as_str()).body(bytes.clone()).send().await.and_then(|r| r.error_for_status()) {
                                failed = Some(format!("uploading {name}: {e}"));
                                break;
                            }
                        }
                        preuploaded.push(json!({
                            "id": idx, "filename": name, "upload_filename": key,
                            "file_size": bytes.len(), "content_type": ctype,
                            "flags": if spoiler { IS_SPOILER } else { 0 },
                        }));
                    }
                    if let Some(err) = failed {
                        return Err(err);
                    }
                    if !to_complete.is_empty() {
                        handle.api.post(&format!("/channels/{channel}/attachments/complete"), &json!({ "uploads": to_complete })).await.map_err(|e| noted(&handle, &channel, e))?;
                    }
                    let mut payload = payload(&handle, &channel, caption, reply_to);
                    payload["attachments"] = json!(preuploaded);
                    let sent = handle.api.post(&format!("/channels/{channel}/messages"), &payload).await.map_err(|e| noted(&handle, &channel, e))?;
                    messages::record(state, &handle, &sent);
                    return Ok(());
                }
            }
            Err(super::api::ApiError::Refused { code, .. }) if code == "FEATURE_TEMPORARILY_DISABLED" => {}
            Err(e) => return Err(noted(&handle, &channel, e)),
        }
    }
    let mut payload = payload(&handle, &channel, caption, reply_to);
    payload["attachments"] = json!(listed);
    let sent = handle.api.multipart(Method::POST, &format!("/channels/{channel}/messages"), &payload, &parts).await.map_err(|e| noted(&handle, &channel, e))?;
    messages::record(state, &handle, &sent);
    Ok(())
}

/// Guild stickers available to this account, in the sticker picker's shape.
pub fn list_stickers(handle: &Handle, buffer: &Buffer) -> Value {
    let channel = buffer.remote_id.as_deref();
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let here = channel.and_then(|c| world.channels.get(c)).and_then(|c| c.guild.clone());
    let mut rows: Vec<(bool, String, Value)> = world
        .stickers
        .iter()
        .map(|(id, s)| {
            let pack = world.guilds.get(&s.guild).map(|g| g.name.clone()).unwrap_or_else(|| "Community".to_string());
            let url = super::media::sticker_url(&handle.instance, id, s.animated).map(|u| super::media::cached(&handle.account_id, super::media::MEDIA, &u, None, Some("image/webp")));
            let local = here.as_deref() == Some(s.guild.as_str());
            (
                !local,
                format!("{pack}\0{}", s.name.to_lowercase()),
                json!({ "id": id, "name": s.name, "pack": pack, "body": s.name, "url": url }),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    Value::Array(rows.into_iter().map(|(_, _, v)| v).collect())
}

/// Sends a sticker by id (`POST /channels/{channel}/messages` with `sticker_ids`).
pub async fn send_sticker(state: &AppState, buffer: &Buffer, sticker_id: &str) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    may_write(&handle, &channel)?;
    slowed(&handle, &channel)?;
    let sent = handle.api.post(&format!("/channels/{channel}/messages"), &json!({ "sticker_ids": [sticker_id] })).await.map_err(|e| noted(&handle, &channel, e))?;
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
    // The newest message held here, or the newest Fluxer says the channel has where that is later: a channel marked read
    // without being opened holds nothing, and acknowledging nothing left it unread in Fluxer's own app (seen on
    // fluxer.com, 2026-11-18).
    let held = state.store.newest_message_id(&buffer.id).ok().flatten().and_then(|id| id.parse::<u64>().ok());
    let told = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(&channel).and_then(|c| c.last_message_id.clone()).and_then(|id| id.parse::<u64>().ok());
    let newest = held.max(told).map(|n| n.to_string());
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
