//! Threads and forums (`http-api/threads.mdx`, `forums.mdx`, `gateway/threads.md`).
//!
//! A thread is a conversation that hangs off a channel; a forum is a channel that is only a list of posts, each a thread
//! with a first message. The window's own pieces for Discord's - the list of a channel's threads, the forum pane with
//! its cards and tags, posting - ask the same questions, and Fluxer's answers have Discord's shapes, so the cards are
//! made by Discord's own `post_card` and only the pictures are put right (they live on Fluxer's media proxy).
//!
//! A thread becomes a conversation here once this account has joined it: opening one joins it, as Fluxer's own app does,
//! since a thread's messages go only to its members.

use super::world::Kind;
use super::{events, messages, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

const PAGE: usize = 25;

fn channel_of(buffer: &Buffer) -> Result<&str> {
    buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Fluxer conversation"))
}

/// A channel's threads, newest activity first: the thread search, which answers a user for live and archived threads
/// alike (the guild's list of live threads is for bots only).
pub async fn list(handle: &Handle, buffer: &Buffer) -> Result<Value> {
    let channel = channel_of(buffer)?;
    let query = vec![("sort_by", "last_message_time".to_string()), ("sort_order", "desc".to_string()), ("limit", PAGE.to_string())];
    let answer = handle.api.get(&format!("/channels/{channel}/threads/search"), &query).await.map_err(|e| anyhow!("{e}"))?;
    let mut rows: Vec<Value> = answer["threads"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| json!({ "id": t["id"], "name": t["name"], "archived": t["thread_metadata"]["archived"].as_bool().unwrap_or(false), "messageCount": t["message_count"].as_i64().unwrap_or(0) }))
        .collect();
    // The search is filled a moment after a thread is made; the threads the gateway has told of are added meanwhile.
    let known: Vec<Value> = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world.channels.values().filter(|c| c.kind == Kind::Thread && c.parent.as_deref() == Some(channel)).map(|c| json!({ "id": c.id, "name": c.name, "archived": false, "messageCount": 0 })).collect()
    };
    for k in known {
        if !rows.iter().any(|r| r["id"] == k["id"]) {
            rows.push(k);
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r["id"].as_str().and_then(|i| i.parse::<u64>().ok()).unwrap_or(0)));
    Ok(json!({ "threads": rows }))
}

/// A post's card with its author's picture from the media proxy instead of Discord's CDN.
fn card(handle: &Handle, thread: &Value, first: Option<&Value>, tags: &[Value]) -> Value {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let own = world.own_id.clone();
    let name = world.name_in(None, &own);
    drop(world);
    let mut card = crate::backend::discord::forums::post_card(thread, first, tags, &own, Some(&name));
    card["avatarUrl"] = first.and_then(|m| messages::avatar_of(handle, &m["author"])).map(Value::String).unwrap_or(Value::Null);
    card
}

/// A page of a forum's posts, newest activity first (or newest made, for `created`).
pub async fn forum_posts(handle: &Handle, buffer: &Buffer, sort: &str, offset: usize) -> Result<Value> {
    let channel = channel_of(buffer)?;
    let tags = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(channel).map(|c| c.tags.clone()).unwrap_or_default();
    let sort_by = if sort == "created" { "creation_time" } else { "last_message_time" };
    let query = vec![("sort_by", sort_by.to_string()), ("sort_order", "desc".to_string()), ("limit", PAGE.to_string()), ("offset", offset.to_string())];
    let answer = handle.api.get(&format!("/channels/{channel}/threads/search"), &query).await.map_err(|e| anyhow!("{e}"))?;
    let firsts = answer["first_messages"].as_array().cloned().unwrap_or_default();
    let posts: Vec<Value> = answer["threads"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            let id = t["id"].as_str().unwrap_or_default();
            let first = firsts.iter().find(|m| m["channel_id"].as_str() == Some(id));
            card(handle, t, first, &tags)
        })
        .collect();
    let total = answer["total_results"].as_u64().map(|n| n as usize).unwrap_or(posts.len());
    Ok(json!({ "posts": posts, "hasMore": answer["has_more"].as_bool().unwrap_or(false), "total": total, "tags": crate::backend::discord::forums::tag_list(&tags) }))
}

/// Joins a thread and makes it a conversation, with its recent history; answers the conversation.
pub async fn open(state: &AppState, handle: &Handle, thread: &str) -> Result<Value> {
    if let Err(e) = handle.api.put(&format!("/channels/{thread}/thread-members/@me"), None).await {
        tracing::debug!("fluxer: joining thread {thread}: {e}");
    }
    let got = handle.api.get(&format!("/channels/{thread}"), &[]).await.map_err(|e| anyhow!("{e}"))?;
    let channel = {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let id = world.put_channel(&got).ok_or_else(|| anyhow!("Fluxer did not describe that thread"))?;
        let c = world.channels.get_mut(&id).ok_or_else(|| anyhow!("Fluxer did not describe that thread"))?;
        c.joined = true;
        c.clone()
    };
    if channel.kind != Kind::Thread {
        bail!("that is not a thread");
    }
    // Filed under its channel's heading with the rest of the guild.
    match channel.guild.as_deref() {
        Some(g) => events::register_guild_channels(state, handle, g),
        None => events::register_channel(state, handle, &channel, 0, None),
    }
    let buffer = messages::buffer_of(state, &handle.account_id, &channel.id).ok_or_else(|| anyhow!("the thread did not come up"))?;
    let _ = messages::history(state, handle, &buffer, None).await;
    Ok(json!({ "bufferId": buffer.id }))
}

/// Makes a post in a forum: a thread with its first message. Answers the conversation it opens.
pub async fn create_post(state: &AppState, handle: &Handle, buffer: &Buffer, title: &str, body: &str, tags: &[String]) -> Result<Value> {
    let channel = channel_of(buffer)?;
    let title = title.trim();
    if title.is_empty() {
        bail!("a post needs a title");
    }
    if body.trim().is_empty() {
        bail!("a post needs something to say");
    }
    let content = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let guild = world.channels.get(channel).and_then(|c| c.guild.clone());
        messages::outgoing(&world, guild.as_deref(), body)
    };
    let mut payload = json!({ "name": title.chars().take(100).collect::<String>(), "message": { "content": content } });
    if !tags.is_empty() {
        payload["applied_tags"] = json!(tags);
    }
    let made = handle.api.post(&format!("/channels/{channel}/threads"), &payload).await.map_err(|e| anyhow!("{e}"))?;
    let id = made["id"].as_str().ok_or_else(|| anyhow!("Fluxer made a post with no id"))?;
    open(state, handle, id).await
}

/// Starts a thread from a message, and opens it.
pub async fn start_from(state: &AppState, handle: &Handle, buffer: &Buffer, message: &str, name: &str) -> Result<Value> {
    let channel = channel_of(buffer)?;
    let made = handle.api.post(&format!("/channels/{channel}/messages/{message}/threads"), &json!({ "name": name.trim().chars().take(100).collect::<String>() })).await.map_err(|e| anyhow!("{e}"))?;
    let id = made["id"].as_str().ok_or_else(|| anyhow!("Fluxer made a thread with no id"))?;
    open(state, handle, id).await
}
