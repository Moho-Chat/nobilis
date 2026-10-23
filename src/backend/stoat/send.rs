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

fn words(e: super::api::ApiError) -> String {
    e.to_string()
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
    let mut payload = json!({ "content": body, "nonce": format!("moho-{}", crate::model::next_message_id()) });
    if let Some(reply) = reply_to.filter(|r| !r.is_empty()) {
        payload["replies"] = json!([{ "id": reply, "mention": false }]);
    }
    let sent = handle.api.post(&format!("/channels/{channel}/messages"), &payload).await.map_err(words)?;
    // At once rather than when the event comes back: the echo finds it kept already.
    messages::record(state, &handle, &sent);
    Ok(())
}

pub async fn edit(state: &AppState, buffer: &Buffer, message_id: &str, body: &str) -> Result<(), String> {
    let (handle, channel) = handle_for(state, buffer)?;
    check(body, message_limit(&handle, chrono::Utc::now().timestamp_millis()))?;
    handle.api.patch(&format!("/channels/{channel}/messages/{message_id}"), &json!({ "content": body })).await.map_err(words)?;
    let text = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let server = world.channels.get(&channel).and_then(|c| c.server.clone());
        messages::words(&world, server.as_deref(), body)
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
        let name = emoji.trim_matches(':');
        world.emojis.iter().find(|(_, n)| n.as_str() == name).map(|(id, _)| id.clone()).unwrap_or_else(|| emoji.to_string())
    };
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
    fn an_emoji_is_written_in_an_address_the_way_the_server_reads_it() {
        assert_eq!(urlencode("👍"), "%F0%9F%91%8D");
        assert_eq!(urlencode("01EMOJ000000000000000000AA"), "01EMOJ000000000000000000AA");
    }
}
