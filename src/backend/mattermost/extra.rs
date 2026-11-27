//! What a channel has beyond its posts: pins, saved posts, search, reading around one post, the threads the account
//! follows, drafts, scheduled posts, bookmarks and slash commands.

use super::{messages, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

fn channel_of(buffer: &Buffer) -> Result<String> {
    buffer.remote_id.clone().ok_or_else(|| anyhow!("this is not a Mattermost conversation"))
}

fn err(e: super::api::ApiError) -> anyhow::Error {
    anyhow!("{e}")
}

/// One post as the window reads a row of results, pins or saved posts.
pub fn row(handle: &Handle, buffer_id: &str, post: &Value) -> Option<Value> {
    let id = post["id"].as_str()?;
    let shaped = messages::shape(handle, post);
    let body = if shaped.body.trim().is_empty() { "(no text)".to_string() } else { shaped.body };
    Some(json!({
        "id": id,
        "bufferId": buffer_id,
        "from": shaped.from,
        "body": body,
        "ts": post["create_at"].as_i64().unwrap_or(0) / 1000,
        "kind": if shaped.system { "system" } else { "chat" },
        "isAction": false,
        "isHighlight": false,
        "edited": post["edit_at"].as_i64().unwrap_or(0) > 0,
        "isOwn": post["user_id"].as_str() == Some(handle.own_id().as_str()),
    }))
}

/// The buffer a post's channel is, for a row from anywhere on the server.
fn buffer_for(state: &AppState, handle: &Handle, post: &Value) -> Option<Buffer> {
    messages::buffer_of(state, &handle.account_id, post["channel_id"].as_str()?)
}

/// The team a conversation is searched in: its own, or (for a direct or group message, which belongs to none) the
/// first the account is in.
fn team_for(handle: &Handle, channel: &str) -> String {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    world.channels.get(channel).map(|c| c.team_id.clone()).filter(|t| !t.is_empty()).or_else(|| world.team_order.first().cloned()).unwrap_or_default()
}

/// The server's search, in one conversation (`in:` it) or everywhere: the words as typed, with Mattermost's own
/// `from:`, `in:`, `on:`, `before:` and `after:` understood by the server.
pub async fn search(state: &AppState, handle: &Handle, buffer: Option<&Buffer>, query: &str, limit: usize) -> Result<Value> {
    let (team, terms) = match buffer {
        Some(buffer) => {
            let channel = channel_of(buffer)?;
            // A direct message is searched in by the other person's name, as Mattermost's own box writes it.
            let name = {
                let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                world.channels.get(&channel).map(|c| match c.other(&world.own_id).and_then(|o| world.username_of(&o)) {
                    Some(username) => format!("@{username}"),
                    None => c.name.clone(),
                }).unwrap_or_default()
            };
            let scoped = if query.contains("in:") || name.is_empty() { query.to_string() } else { format!("{query} in:{name}") };
            (team_for(handle, &channel), scoped)
        }
        None => (handle.world.lock().unwrap_or_else(|e| e.into_inner()).team_order.first().cloned().unwrap_or_default(), query.to_string()),
    };
    let body = json!({ "terms": terms, "is_or_search": false, "include_deleted_channels": false, "time_zone_offset": 0, "page": 0, "per_page": limit.clamp(1, 100) });
    let path = if team.is_empty() { "/api/v4/posts/search".to_string() } else { format!("/api/v4/teams/{team}/posts/search") };
    let answer = handle.api.post(&path, &body).await.map_err(err)?;
    let posts = answer_posts(&answer);
    messages::learn_authors(handle, &posts.iter().collect::<Vec<_>>()).await;
    let mut names = serde_json::Map::new();
    let mut results = Vec::new();
    for post in posts.iter().rev() {
        let Some(b) = buffer_for(state, handle, post) else { continue };
        names.insert(b.id.clone(), json!(b.name));
        results.extend(row(handle, &b.id, post));
    }
    let count = results.len();
    Ok(json!({ "results": results, "roomNames": names, "count": count }))
}

/// The posts of an answer shaped `{order, posts}`, newest first as the server orders them.
fn answer_posts(answer: &Value) -> Vec<Value> {
    let posts = &answer["posts"];
    answer["order"].as_array().into_iter().flatten().filter_map(|id| id.as_str()).filter_map(|id| posts.get(id).cloned()).collect()
}

/// The files found for some words, in a team.
pub async fn search_files(handle: &Handle, team: &str, query: &str) -> Result<Value> {
    let answer = handle.api.post(&format!("/api/v4/teams/{team}/files/search"), &json!({ "terms": query, "is_or_search": false, "page": 0, "per_page": 50 })).await.map_err(err)?;
    let infos = &answer["file_infos"];
    let files: Vec<Value> = answer["order"].as_array().into_iter().flatten().filter_map(|id| id.as_str()).filter_map(|id| infos.get(id)).filter_map(|f| messages::attachment_of(&handle.account_id, f).map(|a| json!({ "postId": f["post_id"], "channelId": f["channel_id"], "file": a }))).collect();
    Ok(json!({ "files": files }))
}

/// What a channel has pinned, newest first.
pub async fn pins(handle: &Handle, buffer: &Buffer) -> Result<Value> {
    let channel = channel_of(buffer)?;
    let answer = handle.api.get(&format!("/api/v4/channels/{channel}/pinned"), &[]).await.map_err(err)?;
    let posts = answer_posts(&answer);
    messages::learn_authors(handle, &posts.iter().collect::<Vec<_>>()).await;
    let pinned: Vec<Value> = posts.iter().filter_map(|p| row(handle, &buffer.id, p)).collect();
    Ok(json!({ "bufferId": buffer.id, "pinned": pinned }))
}

/// Pins a post, or takes the pin off.
pub async fn set_pinned(state: &AppState, handle: &Handle, buffer: &Buffer, post: &str, pinned: bool) -> Result<()> {
    handle.api.post(&format!("/api/v4/posts/{post}/{}", if pinned { "pin" } else { "unpin" }), &json!({})).await.map_err(err)?;
    announce_pins(state, handle, buffer);
    Ok(())
}

/// Tells every window which posts a channel has pinned, after a pin was made or taken off.
pub fn announce_pins(state: &AppState, handle: &Handle, buffer: &Buffer) {
    let (state, handle, buffer) = (state.clone(), handle.clone(), buffer.clone());
    tokio::spawn(async move {
        if let Ok(answer) = pins(&handle, &buffer).await {
            let ids: Vec<String> = answer["pinned"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(str::to_string)).collect();
            state.events.emit("pinnedMessages", json!({ "bufferId": buffer.id, "pinned": ids }));
        }
    });
}

/// Saves a post for later (Mattermost's "Saved", once "Flagged"), or unsaves it.
pub async fn set_saved(handle: &Handle, post: &str, saved: bool) -> Result<()> {
    let own = handle.own_id();
    let preference = json!([{ "user_id": own, "category": "flagged_post", "name": post, "value": "true" }]);
    if saved {
        handle.api.put(&format!("/api/v4/users/{own}/preferences"), &preference).await.map_err(err)?;
        handle.world.lock().unwrap_or_else(|e| e.into_inner()).flagged.insert(post.to_string());
    } else {
        handle.api.post(&format!("/api/v4/users/{own}/preferences/delete"), &preference).await.map_err(err)?;
        handle.world.lock().unwrap_or_else(|e| e.into_inner()).flagged.remove(post);
    }
    Ok(())
}

/// The account's saved posts, newest first, from every conversation.
pub async fn saved(state: &AppState, handle: &Handle) -> Result<Value> {
    let own = handle.own_id();
    let answer = handle.api.get(&format!("/api/v4/users/{own}/posts/flagged"), &[("per_page", "100".into())]).await.map_err(err)?;
    let posts = answer_posts(&answer);
    messages::learn_authors(handle, &posts.iter().collect::<Vec<_>>()).await;
    let rows: Vec<Value> = posts.iter().filter_map(|p| row(handle, &buffer_for(state, handle, p)?.id, p)).collect();
    Ok(json!({ "posts": rows, "ids": handle.world.lock().unwrap_or_else(|e| e.into_inner()).flagged.iter().cloned().collect::<Vec<_>>() }))
}

/// The account's saved posts changed (from here or another app): the window is told which they are.
pub fn saved_changed(state: &AppState, handle: &Handle) {
    let ids: Vec<String> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).flagged.iter().cloned().collect();
    state.events.emit("mattermostSaved", json!({ "accountId": handle.account_id, "ids": ids }));
}

/// Reads the part of a channel around one post and keeps it; answers when that post was made.
pub async fn context(state: &AppState, handle: &Handle, buffer: &Buffer, post: &str) -> Result<i64> {
    let channel = channel_of(buffer)?;
    let target = handle.api.get(&format!("/api/v4/posts/{post}"), &[]).await.map_err(err)?;
    let half = (messages::PAGE / 2).to_string();
    let path = format!("/api/v4/channels/{channel}/posts");
    let (before_q, after_q) = ([("before", post.to_string()), ("per_page", half.clone())], [("after", post.to_string()), ("per_page", half)]);
    let (before, after) = tokio::join!(handle.api.get(&path, &before_q), handle.api.get(&path, &after_q));
    let mut all = vec![target.clone()];
    for page in [before, after].into_iter().flatten() {
        all.extend(messages::ordered(&page));
    }
    all.sort_by_key(|p| p["create_at"].as_i64().unwrap_or(0));
    messages::learn_authors(handle, &all.iter().collect::<Vec<_>>()).await;
    for p in &all {
        messages::record(state, handle, p);
    }
    target["create_at"].as_i64().map(|ms| ms / 1000).ok_or_else(|| anyhow!("the server did not say when that was posted"))
}

/// Reads forward from a post, towards the present; answers how many were new.
pub async fn newer(state: &AppState, handle: &Handle, buffer: &Buffer, post: &str) -> Result<usize> {
    let channel = channel_of(buffer)?;
    let page = handle.api.get(&format!("/api/v4/channels/{channel}/posts"), &[("after", post.to_string()), ("per_page", messages::PAGE.to_string())]).await.map_err(err)?;
    let list = messages::ordered(&page);
    messages::learn_authors(handle, &list.iter().collect::<Vec<_>>()).await;
    Ok(list.iter().filter(|p| messages::record(state, handle, p)).count())
}

/// Who reacted to a post with one emoji.
pub async fn reactors(handle: &Handle, post: &str, shown: &str) -> Result<Value> {
    let name = super::emoji::wire_name(shown);
    let list = handle.api.get(&format!("/api/v4/posts/{post}/reactions"), &[]).await.map_err(err)?;
    let ids: Vec<String> = list.as_array().into_iter().flatten().filter(|r| r["emoji_name"].as_str() == Some(name.as_str())).filter_map(|r| r["user_id"].as_str().map(str::to_string)).collect();
    messages::learn_authors(handle, &ids.iter().map(|id| json!({ "user_id": id })).collect::<Vec<_>>().iter().collect::<Vec<_>>()).await;
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    Ok(json!({ "users": ids.iter().map(|id| json!({ "id": id, "name": world.name_of(id, &handle.server.teammate_name) })).collect::<Vec<_>>() }))
}

/// The threads the account follows in a team, with how many replies each has that it has not read.
pub async fn threads(state: &AppState, handle: &Handle, team: &str, unread_only: bool) -> Result<Value> {
    let own = handle.own_id();
    let mut query = vec![("per_page", "50".to_string()), ("extended", "true".to_string())];
    if unread_only {
        query.push(("unread", "true".to_string()));
    }
    let answer = handle.api.get(&format!("/api/v4/users/{own}/teams/{team}/threads"), &query).await.map_err(err)?;
    let mut out = Vec::new();
    for t in answer["threads"].as_array().into_iter().flatten() {
        let post = &t["post"];
        messages::learn_authors(handle, &[post]).await;
        let Some(buffer) = buffer_for(state, handle, post) else { continue };
        out.push(json!({
            "id": t["id"],
            "bufferId": buffer.id,
            "replies": t["reply_count"],
            "unreadReplies": t["unread_replies"],
            "unreadMentions": t["unread_mentions"],
            "lastReply": t["last_reply_at"].as_i64().map(|ms| ms / 1000),
            "root": row(handle, &buffer.id, post),
        }));
    }
    Ok(json!({ "threads": out, "unreadThreads": answer["total_unread_threads"], "unreadMentions": answer["total_unread_mentions"] }))
}

/// Follows a thread, or stops following it.
pub async fn follow(handle: &Handle, team: &str, thread: &str, on: bool) -> Result<()> {
    let own = handle.own_id();
    let path = format!("/api/v4/users/{own}/teams/{team}/threads/{thread}/following");
    if on {
        handle.api.put(&path, &json!({})).await.map_err(err)?;
    } else {
        handle.api.delete(&path).await.map_err(err)?;
    }
    Ok(())
}

/// Says a thread was read up to now.
pub async fn read_thread(handle: &Handle, team: &str, thread: &str) -> Result<()> {
    let own = handle.own_id();
    let now = chrono::Utc::now().timestamp_millis();
    handle.api.put(&format!("/api/v4/users/{own}/teams/{team}/threads/{thread}/read/{now}"), &json!({})).await.map_err(err)?;
    Ok(())
}

/// A followed thread changed: the window is told, so its list of threads can be asked for again.
pub fn thread_event(state: &AppState, handle: &Handle, kind: &str, data: &Value) {
    let thread = match &data["thread"] {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        other => other.clone(),
    };
    let id = thread["id"].as_str().or(data["thread_id"].as_str()).unwrap_or("");
    state.events.emit("mattermostThread", json!({ "accountId": handle.account_id, "kind": kind, "threadId": id, "unreadReplies": thread["unread_replies"], "unreadMentions": thread["unread_mentions"] }));
}

/// The account's drafts in a team (with direct messages), as the server keeps them for every app.
pub async fn drafts(state: &AppState, handle: &Handle, team: &str) -> Result<Value> {
    let own = handle.own_id();
    let list = handle.api.get(&format!("/api/v4/users/{own}/teams/{team}/drafts"), &[]).await.map_err(err)?;
    let out: Vec<Value> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            let buffer = messages::buffer_of(state, &handle.account_id, d["channel_id"].as_str()?)?;
            Some(json!({ "bufferId": buffer.id, "rootId": d["root_id"], "text": d["message"], "updated": d["update_at"].as_i64().map(|ms| ms / 1000) }))
        })
        .collect();
    Ok(json!({ "drafts": out }))
}

/// Keeps a draft at the server (an empty one removes it), so the account's other apps have it.
pub async fn save_draft(handle: &Handle, buffer: &Buffer, root: Option<&str>, text: &str) -> Result<()> {
    if !handle.server.synced_drafts {
        return Ok(());
    }
    let channel = channel_of(buffer)?;
    let own = handle.own_id();
    if text.trim().is_empty() {
        let path = match root.filter(|r| !r.is_empty()) {
            Some(root) => format!("/api/v4/users/{own}/channels/{channel}/drafts/{root}"),
            None => format!("/api/v4/users/{own}/channels/{channel}/drafts"),
        };
        handle.api.delete(&path).await.map_err(err)?;
    } else {
        handle.api.post("/api/v4/drafts", &json!({ "user_id": own, "channel_id": channel, "root_id": root.unwrap_or(""), "message": text, "create_at": 0, "update_at": chrono::Utc::now().timestamp_millis() })).await.map_err(err)?;
    }
    Ok(())
}

/// A draft made, changed or removed on another of the account's apps: the window is told.
pub fn draft_event(state: &AppState, handle: &Handle, kind: &str, data: &Value) {
    let draft = match &data["draft"] {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        other => other.clone(),
    };
    let Some(buffer) = draft["channel_id"].as_str().and_then(|c| messages::buffer_of(state, &handle.account_id, c)) else { return };
    let text = if kind == "draft_deleted" { "" } else { draft["message"].as_str().unwrap_or("") };
    state.events.emit("serviceDraft", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "rootId": draft["root_id"], "text": text }));
}

/// The posts the account has scheduled to go later, in a team.
pub async fn scheduled(state: &AppState, handle: &Handle, team: &str) -> Result<Value> {
    if !handle.server.at_least(10, 3) {
        return Err(anyhow!("This server is older than scheduled posts (Mattermost 10.3)"));
    }
    if !handle.server.licensed {
        return Err(anyhow!("Scheduled posts need a Mattermost licence, which this server does not have"));
    }
    let answer = handle.api.get(&format!("/api/v4/posts/scheduled/team/{team}"), &[("includeDirectChannels", "true".into())]).await.map_err(err)?;
    let mut out = Vec::new();
    for list in answer.as_object().map(|o| o.values().cloned().collect::<Vec<_>>()).unwrap_or_default() {
        for p in list.as_array().into_iter().flatten() {
            let buffer = p["channel_id"].as_str().and_then(|c| messages::buffer_of(state, &handle.account_id, c));
            out.push(json!({ "id": p["id"], "bufferId": buffer.map(|b| b.id), "text": p["message"], "at": p["scheduled_at"].as_i64().map(|ms| ms / 1000), "error": p["error_code"] }));
        }
    }
    Ok(json!({ "scheduled": out }))
}

/// Schedules a post for later (`at` in Unix seconds).
pub async fn schedule(handle: &Handle, buffer: &Buffer, text: &str, root: Option<&str>, at: i64) -> Result<Value> {
    if !handle.server.at_least(10, 3) {
        return Err(anyhow!("This server is older than scheduled posts (Mattermost 10.3)"));
    }
    if !handle.server.licensed {
        return Err(anyhow!("Scheduled posts need a Mattermost licence, which this server does not have"));
    }
    if at * 1000 <= chrono::Utc::now().timestamp_millis() {
        return Err(anyhow!("That time has passed"));
    }
    let channel = channel_of(buffer)?;
    let body = json!({ "channel_id": channel, "message": text, "root_id": root.unwrap_or(""), "scheduled_at": at * 1000, "user_id": handle.own_id() });
    handle.api.post("/api/v4/posts/schedule", &body).await.map_err(err)
}

/// Takes a scheduled post back.
pub async fn unschedule(handle: &Handle, id: &str) -> Result<()> {
    handle.api.delete(&format!("/api/v4/posts/schedule/{id}")).await.map_err(err)?;
    Ok(())
}

/// A channel's bookmarks: links and files along the top.
pub async fn bookmarks(handle: &Handle, buffer: &Buffer) -> Result<Value> {
    let channel = channel_of(buffer)?;
    // Bookmarks came in 9.11, and need a licence.
    if !handle.server.at_least(9, 11) || !handle.server.licensed {
        return Ok(json!({ "bookmarks": [] }));
    }
    let list = handle.api.get(&format!("/api/v4/channels/{channel}/bookmarks"), &[]).await.map_err(err)?;
    let out: Vec<Value> = list
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| {
            json!({
                "id": b["id"],
                "name": b["display_name"],
                "url": b["link_url"],
                "emoji": b["emoji"],
                "kind": b["type"],
                "file": b["file"].is_object().then(|| messages::attachment_of(&handle.account_id, &b["file"])).flatten(),
            })
        })
        .collect();
    Ok(json!({ "bookmarks": out }))
}

/// Adds a link bookmark, changes one, or removes one.
pub async fn set_bookmark(handle: &Handle, buffer: &Buffer, id: Option<&str>, name: &str, url: &str, remove: bool) -> Result<()> {
    let channel = channel_of(buffer)?;
    if !handle.server.licensed {
        return Err(anyhow!("Channel bookmarks need a Mattermost licence, which this server does not have"));
    }
    match (id, remove) {
        (Some(id), true) => handle.api.delete(&format!("/api/v4/channels/{channel}/bookmarks/{id}")).await.map(|_| ()),
        (Some(id), false) => handle.api.request(reqwest::Method::PATCH, &format!("/api/v4/channels/{channel}/bookmarks/{id}"), &[], Some(&json!({ "display_name": name, "link_url": url }))).await.map(|_| ()),
        (None, _) => handle.api.post(&format!("/api/v4/channels/{channel}/bookmarks"), &json!({ "channel_id": channel, "display_name": name, "link_url": url, "type": "link" })).await.map(|_| ()),
    }
    .map_err(err)
}

/// A channel's bookmarks changed: the window is told.
pub fn bookmark_event(state: &AppState, handle: &Handle, channel: &str) {
    if let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) {
        state.events.emit("mattermostBookmarks", json!({ "accountId": handle.account_id, "bufferId": buffer.id }));
    }
}

/// The slash commands a team offers, matching what has been typed.
pub async fn commands(handle: &Handle, buffer: &Buffer, typed: &str) -> Result<Vec<Value>> {
    let channel = channel_of(buffer)?;
    let team = team_for(handle, &channel);
    if team.is_empty() {
        return Ok(Vec::new());
    }
    let list = handle.api.get(&format!("/api/v4/teams/{team}/commands/autocomplete"), &[]).await.map_err(err)?;
    let wanted = typed.trim_start_matches('/').to_lowercase();
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["trigger"].as_str().is_some_and(|t| t.to_lowercase().starts_with(&wanted)))
        .map(|c| json!({ "name": c["trigger"], "usage": format!("/{} {}", c["trigger"].as_str().unwrap_or(""), c["auto_complete_hint"].as_str().unwrap_or("")).trim(), "description": c["auto_complete_desc"], "source": c["display_name"].as_str().filter(|d| !d.is_empty()).unwrap_or("Mattermost"), "kind": "service" }))
        .collect())
}

/// Runs a slash command in a conversation, as Mattermost's app does: the server (or the integration behind the
/// command) answers by posting, or with words just for this account, which are shown as a notice.
pub async fn run_command(state: &AppState, handle: &Handle, buffer: &Buffer, line: &str, root: Option<&str>) -> Result<()> {
    let channel = channel_of(buffer)?;
    let team = team_for(handle, &channel);
    let answer = handle.api.post("/api/v4/commands/execute", &json!({ "channel_id": channel, "team_id": team, "command": line, "root_id": root.unwrap_or(""), "parent_id": root.unwrap_or("") })).await.map_err(err)?;
    if let Some(text) = answer["text"].as_str().filter(|t| !t.is_empty()) {
        if answer["response_type"].as_str() != Some("in_channel") {
            state.runtime.record_message(state, &buffer.account_id, &buffer.name, &buffer.kind, "Mattermost", text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
        }
    }
    if let Some(goto) = answer["goto_location"].as_str().filter(|g| !g.is_empty()) {
        state.events.emit("openUrl", json!({ "url": goto }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_of_posts_is_read_in_the_servers_order() {
        let answer = json!({ "order": ["b", "a"], "posts": { "a": { "id": "a" }, "b": { "id": "b" } } });
        assert_eq!(answer_posts(&answer).iter().map(|p| p["id"].as_str().unwrap()).collect::<Vec<_>>(), vec!["b", "a"]);
        assert!(answer_posts(&json!({})).is_empty());
    }
}
