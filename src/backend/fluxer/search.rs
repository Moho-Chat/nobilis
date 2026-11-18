//! Looking things up in a channel: searching its history, its pinned messages, and reading the part of it around a
//! message that is older than anything held.
//!
//! Search is `POST /v1/search/messages` with the channel as its context (`http-api/search.mdx`). An instance may answer
//! that the channel is still being indexed; that is said in words rather than as no results. A hit is a row and
//! nothing is recorded; pressing one reads the messages around it (`context`), the way a jump to anything old does.

use super::messages;
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

fn channel_of(buffer: &Buffer) -> Result<&str> {
    buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Fluxer conversation"))
}

/// One message as the window reads a row of results or pins.
pub fn row(handle: &Handle, buffer_id: &str, message: &Value) -> Option<Value> {
    let id = message["id"].as_str()?;
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let author = message["author"]["id"].as_str().unwrap_or("");
    let guild = world.channels.get(message["channel_id"].as_str().unwrap_or("")).and_then(|c| c.guild.clone());
    let system = !matches!(message["type"].as_i64().unwrap_or(0), 0 | 19 | 20 | 21 | 23);
    let from = if world.users.contains_key(author) { world.name_in(guild.as_deref(), author) } else { message["author"]["global_name"].as_str().or(message["author"]["username"].as_str()).unwrap_or("someone").to_string() };
    let body = if system { messages::system_text(&world, guild.as_deref(), message).unwrap_or_default() } else { messages::words(&world, Some(handle), guild.as_deref(), message["content"].as_str().unwrap_or("")) };
    let body = if body.trim().is_empty() { "(no text)".to_string() } else { body };
    Some(json!({
        "id": id,
        "bufferId": buffer_id,
        "from": from,
        "body": body,
        "ts": message["timestamp"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp()).or_else(|| messages::snowflake_secs(id)).unwrap_or(0),
        "kind": if system { "system" } else { "chat" },
        "isAction": false,
        "isHighlight": false,
        "edited": message["edited_timestamp"].is_string(),
        "isOwn": author == world.own_id,
    }))
}

/// The matches for some words in one channel, newest first.
pub async fn run(handle: &Handle, buffer: &Buffer, query: &str, limit: usize) -> Result<Value> {
    let channel = channel_of(buffer)?;
    let answer = handle.api.post("/search/messages", &json!({ "context_channel_id": channel, "channel_ids": [channel], "content": query, "hits_per_page": limit.clamp(1, 25) })).await.map_err(|e| anyhow!("{e}"))?;
    if answer["indexing"].as_bool() == Some(true) {
        bail!("Fluxer is still indexing this channel for search. Try again in a little while");
    }
    let results: Vec<Value> = answer["messages"].as_array().into_iter().flatten().filter_map(|m| row(handle, &buffer.id, m)).collect();
    let count = answer["total"].as_u64().map(|n| n as usize).unwrap_or(results.len());
    Ok(json!({ "results": results, "roomNames": { buffer.id.clone(): buffer.name.clone() }, "count": count }))
}

/// What a channel has pinned, newest pin first.
pub async fn pins(handle: &Handle, buffer: &Buffer) -> Result<Value> {
    let channel = channel_of(buffer)?;
    let answer = handle.api.get(&format!("/channels/{channel}/messages/pins"), &[("limit", "50".to_string())]).await.map_err(|e| anyhow!("{e}"))?;
    let pinned: Vec<Value> = answer["items"].as_array().into_iter().flatten().filter_map(|p| row(handle, &buffer.id, &p["message"])).collect();
    Ok(json!({ "bufferId": buffer.id, "pinned": pinned }))
}

/// Tells every window which messages a channel has pinned, after a pin was made or taken off.
pub fn announce(state: &AppState, handle: &Handle, buffer: &Buffer) {
    let (state, handle, buffer) = (state.clone(), handle.clone(), buffer.clone());
    tokio::spawn(async move {
        if let Ok(answer) = pins(&handle, &buffer).await {
            let ids: Vec<String> = answer["pinned"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(str::to_string)).collect();
            state.events.emit("pinnedMessages", json!({ "bufferId": buffer.id, "pinned": ids }));
        }
    });
}

/// Pins a message, or takes the pin off.
pub async fn set_pinned(handle: &Handle, buffer: &Buffer, message: &str, pinned: bool) -> Result<()> {
    let channel = channel_of(buffer)?;
    let path = format!("/channels/{channel}/pins/{message}");
    if pinned { handle.api.put(&path, None).await } else { handle.api.delete(&path).await }.map(|_| ()).map_err(|e| anyhow!("{e}"))
}

/// Reads the part of a channel around one message and keeps it; answers when that message was sent.
pub async fn context(state: &AppState, handle: &Handle, buffer: &Buffer, message: &str) -> Result<i64> {
    let channel = channel_of(buffer)?;
    let answer = handle.api.get(&format!("/channels/{channel}/messages"), &[("around", message.to_string()), ("limit", messages::PAGE.to_string())]).await.map_err(|e| anyhow!("{e}"))?;
    let mut list = answer.as_array().cloned().unwrap_or_default();
    list.sort_by_key(|m| m["id"].as_str().and_then(|i| i.parse::<u64>().ok()).unwrap_or(0));
    for m in &list {
        messages::record(state, handle, m);
    }
    messages::snowflake_secs(message).ok_or_else(|| anyhow!("that is not a Fluxer message"))
}

/// Reads forward from a message, towards the present; answers how many were new.
pub async fn newer(state: &AppState, handle: &Handle, buffer: &Buffer, message: &str) -> Result<usize> {
    let channel = channel_of(buffer)?;
    let answer = handle.api.get(&format!("/channels/{channel}/messages"), &[("after", message.to_string()), ("limit", messages::PAGE.to_string())]).await.map_err(|e| anyhow!("{e}"))?;
    let mut list = answer.as_array().cloned().unwrap_or_default();
    list.sort_by_key(|m| m["id"].as_str().and_then(|i| i.parse::<u64>().ok()).unwrap_or(0));
    Ok(list.iter().filter(|m| messages::record(state, handle, m)).count())
}
