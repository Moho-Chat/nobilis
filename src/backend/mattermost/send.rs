//! Saying things: send (into a channel or a thread), files, edit, delete, react, typing, and telling the server a
//! channel was read.

use super::{emoji, messages, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use serde_json::json;

pub fn handle_for(state: &AppState, buffer: &Buffer) -> Result<(Handle, String), String> {
    let handle = state.runtime.mattermost_handle(&buffer.account_id).ok_or_else(|| "that Mattermost account is not connected".to_string())?;
    let channel = buffer.remote_id.clone().ok_or_else(|| "this is not a Mattermost conversation".to_string())?;
    Ok((handle, channel))
}

fn words(e: super::api::ApiError) -> String {
    e.to_string()
}

/// What a post must be before it is sent: something to say, and no more than the server takes.
pub fn check(body: &str, limit: usize) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("There is nothing to send".to_string());
    }
    let length = body.chars().count();
    if length > limit {
        return Err(format!("That message is too long: this server takes at most {limit} characters, and this has {length}"));
    }
    Ok(())
}

/// Refuses a post where the account may not write, saying why.
fn writable(handle: &Handle, channel: &str) -> Result<(), String> {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    match world.channels.get(channel).and_then(|c| world.read_only(c)) {
        Some(why) => Err(why.to_string()),
        None => Ok(()),
    }
}

/// The thread a post goes into: the one a reply names, or the thread that message is itself in.
fn root_of(state: &AppState, buffer: &Buffer, reply_to: Option<&str>) -> Option<String> {
    let reply = reply_to.filter(|r| super::is_id(r))?;
    // A reply to a reply goes into the same thread: Mattermost's threads are one level deep.
    let stored = state.store.get_message(&buffer.id, reply).ok().flatten();
    Some(stored.and_then(|m| m.reply_to.filter(|r| r.thread).map(|r| r.id)).unwrap_or_else(|| reply.to_string()))
}

/// Sends a post, into the thread of `reply_to` if it names one.
pub async fn send(state: &AppState, buffer: &Buffer, body: &str, reply_to: Option<&str>) -> Result<(), String> {
    send_with(state, buffer, body, reply_to, &[], None).await
}

/// Sends a post with files already uploaded and, optionally, a priority (`important`/`urgent`, with an
/// acknowledgement asked for).
pub async fn send_with(state: &AppState, buffer: &Buffer, body: &str, reply_to: Option<&str>, file_ids: &[String], priority: Option<(&str, bool)>) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    writable(&handle, &channel)?;
    if file_ids.is_empty() {
        check(body, handle.server.max_post)?;
    }
    let mut post = json!({ "channel_id": channel, "message": body, "pending_post_id": format!("{}:{}", handle.own_id(), chrono::Utc::now().timestamp_millis()) });
    if let Some(root) = root_of(state, buffer, reply_to) {
        post["root_id"] = json!(root);
    }
    if !file_ids.is_empty() {
        post["file_ids"] = json!(file_ids);
    }
    if let Some((level, ack)) = priority.filter(|_| handle.server.post_priority) {
        post["metadata"] = json!({ "priority": { "priority": if level == "standard" { "" } else { level }, "requested_ack": ack } });
    }
    let sent = handle.api.post("/api/v4/posts", &post).await.map_err(words)?;
    // At once rather than when the event comes back: the echo finds it kept already.
    messages::record(state, &handle, &sent);
    Ok(())
}

/// What a file's kind of content is, from its name.
pub fn content_type_of(name: &str) -> &'static str {
    crate::backend::stoat::send::content_type_of(name)
}

/// Sends files with a post: each is uploaded to the channel, then named in the post.
pub async fn send_files(state: &AppState, buffer: &Buffer, caption: &str, files: &[(String, bool)], reply_to: Option<&str>, progress: Option<&crate::upload::Progress>) -> Result<(), String> {
    if files.is_empty() {
        return send(state, buffer, caption, reply_to).await;
    }
    let (handle, channel) = handle_for(state, buffer)?;
    writable(&handle, &channel)?;
    if !handle.server.files {
        return Err("This server does not take files".to_string());
    }
    // Mattermost takes up to ten files on a post.
    if files.len() > 10 {
        return Err(format!("Mattermost takes at most 10 files on a post, and this has {}", files.len()));
    }
    let total: u64 = files.iter().filter_map(|(p, _)| std::fs::metadata(p).ok().map(|m| m.len())).sum();
    if let Some(p) = progress {
        p.at(crate::upload::Phase::Preparing, total as usize, "Mattermost");
    }
    let mut parts = Vec::new();
    for (path, _) in files {
        let size = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?.len();
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".to_string());
        if size > handle.server.max_file {
            return Err(format!("{name} is {} MB and this server takes at most {} MB", size / 1_000_000, handle.server.max_file / 1_000_000));
        }
        let bytes = tokio::fs::read(path).await.map_err(|e| format!("reading {path}: {e}"))?;
        parts.push((name.clone(), content_type_of(&name).to_string(), bytes));
    }
    if let Some(p) = progress {
        p.sent(0, total, "Mattermost");
    }
    let answer = handle.api.upload(&channel, parts).await.map_err(words)?;
    let ids: Vec<String> = answer["file_infos"].as_array().into_iter().flatten().filter_map(|f| f["id"].as_str().map(str::to_string)).collect();
    if let Some(p) = progress {
        p.at(crate::upload::Phase::Waiting, total as usize, "Mattermost");
    }
    send_with(state, buffer, caption, reply_to, &ids, None).await
}

pub async fn edit(state: &AppState, buffer: &Buffer, post_id: &str, body: &str) -> Result<(), String> {
    let (handle, _) = handle_for(state, buffer)?;
    check(body, handle.server.max_post)?;
    let post = handle.api.put(&format!("/api/v4/posts/{post_id}/patch"), &json!({ "message": body })).await.map_err(words)?;
    messages::update(state, &handle, &post);
    Ok(())
}

pub async fn delete(state: &AppState, buffer: &Buffer, post_id: &str) -> Result<(), String> {
    let (handle, _) = handle_for(state, buffer)?;
    handle.api.delete(&format!("/api/v4/posts/{post_id}")).await.map_err(words)?;
    state.runtime.delete_message(state, &buffer.id, post_id);
    Ok(())
}

/// Says the account is typing here (in a thread when `parent` names one).
pub fn typing(state: &AppState, buffer: &Buffer, parent: Option<&str>) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    let line = json!({ "action": "user_typing", "seq": 0, "data": { "channel_id": channel, "parent_id": parent.unwrap_or("") } });
    handle.socket.send(line.to_string()).map_err(|_| "the connection is not up".to_string())
}

/// Says the channel was read, so it is read on the account's other apps.
pub async fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
    handle.api.post("/api/v4/channels/members/me/view", &json!({ "channel_id": channel, "prev_channel_id": "", "collapsed_threads_supported": true })).await.map_err(words)?;
    Ok(())
}

/// A reaction put on a post, or taken off.
pub async fn react(state: &AppState, buffer: &Buffer, post_id: &str, shown: &str, add: bool) -> Result<(), String> {
    let (handle, _) = handle_for(state, buffer)?;
    let name = emoji::wire_name(shown);
    if name.is_empty() {
        return Err("Mattermost does not know that emoji by a name".to_string());
    }
    let own = handle.own_id();
    if add {
        // The server's limit on how many different emoji one post may carry, kept to before asking.
        let held: Vec<String> = state.store.get_message(&buffer.id, post_id).ok().flatten().map(|m| m.reactions.iter().map(|r| emoji::wire_name(&r.emoji)).collect()).unwrap_or_default();
        if !held.contains(&name) && held.len() >= handle.server.reaction_limit {
            return Err(format!("A post can carry at most {} different reactions on this server", handle.server.reaction_limit));
        }
        handle.api.post("/api/v4/reactions", &json!({ "user_id": own, "post_id": post_id, "emoji_name": name })).await.map_err(words)?;
    } else {
        handle.api.delete(&format!("/api/v4/users/{own}/posts/{post_id}/reactions/{}", urlencode(&name))).await.map_err(words)?;
    }
    // Not counted here: the server says it to everybody in the channel, this account included, and the event counts it.
    Ok(())
}

/// Acknowledges a post that asked for it, or takes the acknowledgement back.
pub async fn acknowledge(state: &AppState, buffer: &Buffer, post_id: &str, ack: bool) -> Result<(), String> {
    let (handle, _) = handle_for(state, buffer)?;
    let own = handle.own_id();
    let path = format!("/api/v4/users/{own}/posts/{post_id}/ack");
    if ack {
        handle.api.post(&path, &json!({})).await.map_err(words)?;
    } else {
        handle.api.delete(&path).await.map_err(words)?;
    }
    Ok(())
}

pub fn urlencode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_post_needs_something_in_it_and_not_more_than_the_server_takes() {
        assert!(check("hello", 16383).is_ok());
        assert!(check("  ", 16383).unwrap_err().contains("nothing"));
        assert!(check(&"a".repeat(11), 10).unwrap_err().contains("at most 10"));
    }

    #[test]
    fn an_emoji_name_is_written_in_an_address_the_way_the_server_reads_it() {
        assert_eq!(urlencode("+1"), "%2B1");
        assert_eq!(urlencode("party_parrot"), "party_parrot");
    }
}
