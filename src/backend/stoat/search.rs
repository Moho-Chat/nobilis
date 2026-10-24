//! Looking things up in a channel: searching its history, its pinned messages, and reading the part of it around a
//! message that is older than anything held.
//!
//! Stoat searches one channel at a time (there is no search across an account), with the database's own text index, so
//! the query is words and not a filter language. A hit is answered as a message and nothing is recorded; pressing one
//! reads the messages around it (`context`), the way a jump to anything old does.

use super::messages;
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// How many pinned messages are read: a channel with more has more than anybody is going to scroll.
const PINS: usize = 50;

fn channel_of(buffer: &Buffer) -> Result<&str> {
    buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Stoat conversation"))
}

/// The messages in an answer from Stoat (a bare list, or an object that also carries the people), with the people taken
/// into the world so a name can be said.
fn unpack(handle: &Handle, answer: &Value) -> Vec<Value> {
    let (list, users) = match answer {
        Value::Array(list) => (list.clone(), Vec::new()),
        other => (other["messages"].as_array().cloned().unwrap_or_default(), other["users"].as_array().cloned().unwrap_or_default()),
    };
    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    for u in &users {
        if let Some((id, user)) = super::world::parse_user(u) {
            world.users.entry(id).or_insert(user);
        }
    }
    list
}

/// One message as the window reads a row of results or pins.
pub fn row(handle: &Handle, buffer_id: &str, message: &Value) -> Option<Value> {
    let id = message["_id"].as_str()?;
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let author = message["author"].as_str().unwrap_or("");
    let server = world.channels.get(message["channel"].as_str().unwrap_or("")).and_then(|c| c.server.clone());
    let from = message["masquerade"]["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| message["member"]["nickname"].as_str().filter(|n| !n.is_empty()).map(str::to_string))
        .unwrap_or_else(|| world.name_of(author));
    let body = match message.get("system").filter(|s| s.is_object()) {
        Some(system) => messages::system_text(&world, system).unwrap_or_default(),
        None => messages::words(&world, Some((&handle.instance, &handle.account_id)), server.as_deref(), message["content"].as_str().unwrap_or("")),
    };
    let own = author == world.own_id;
    let body = if body.trim().is_empty() { "(no text)".to_string() } else { body };
    Some(json!({
        "id": id,
        "bufferId": buffer_id,
        "from": from,
        "body": body,
        "ts": messages::ulid_ms(id).map(|ms| ms / 1000).unwrap_or(0),
        "kind": if message.get("system").is_some_and(|s| s.is_object()) { "system" } else { "chat" },
        "isAction": false,
        "isHighlight": false,
        "edited": message["edited"].is_string(),
        "isOwn": own,
    }))
}

/// The matches for some words in one channel, newest first.
pub async fn run(handle: &Handle, buffer: &Buffer, query: &str, limit: usize) -> Result<Value> {
    let channel = channel_of(buffer)?;
    let answer = handle.api.post(&format!("/channels/{channel}/search"), &json!({ "query": query, "limit": limit.clamp(1, 100), "include_users": true })).await.map_err(|e| anyhow!("{e}"))?;
    let results: Vec<Value> = unpack(handle, &answer).iter().filter_map(|m| row(handle, &buffer.id, m)).collect();
    let count = results.len();
    Ok(json!({ "results": results, "roomNames": { buffer.id.clone(): buffer.name.clone() }, "count": count }))
}

/// What a channel has pinned, newest first.
pub async fn pins(handle: &Handle, buffer: &Buffer) -> Result<Value> {
    let channel = channel_of(buffer)?;
    let answer = handle.api.post(&format!("/channels/{channel}/search"), &json!({ "pinned": true, "limit": PINS, "include_users": true })).await.map_err(|e| anyhow!("{e}"))?;
    let pinned: Vec<Value> = unpack(handle, &answer).iter().filter_map(|m| row(handle, &buffer.id, m)).collect();
    Ok(json!({ "bufferId": buffer.id, "pinned": pinned }))
}

/// Tells every window which messages a channel has pinned, after a pin was made or taken off. Not said when the list
/// is merely asked for: a window that is told the pins asks again if they differ from what it holds.
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
    let path = format!("/channels/{channel}/messages/{message}/pin");
    if pinned {
        handle.api.post(&path, &json!({})).await
    } else {
        handle.api.delete(&path).await
    }
    .map(|_| ())
    .map_err(|e| anyhow!("{e}"))
}

/// Reads the part of a channel around one message and keeps it; answers when that message was sent.
pub async fn context(state: &AppState, handle: &Handle, buffer: &Buffer, message: &str) -> Result<i64> {
    let channel = channel_of(buffer)?;
    let answer = handle.api.get(&format!("/channels/{channel}/messages"), &[("nearby", message.to_string()), ("limit", messages::PAGE.to_string()), ("include_users", "true".to_string())]).await.map_err(|e| anyhow!("{e}"))?;
    let mut list = unpack(handle, &answer);
    list.sort_by(|a, b| a["_id"].as_str().cmp(&b["_id"].as_str()));
    for m in &list {
        messages::record(state, handle, m);
    }
    messages::ulid_ms(message).map(|ms| ms / 1000).ok_or_else(|| anyhow!("that is not a Stoat message"))
}

/// Reads forward from a message, towards the present; answers how many were new.
pub async fn newer(state: &AppState, handle: &Handle, buffer: &Buffer, message: &str) -> Result<usize> {
    let channel = channel_of(buffer)?;
    let answer = handle.api.get(&format!("/channels/{channel}/messages"), &[("after", message.to_string()), ("sort", "Oldest".to_string()), ("limit", messages::PAGE.to_string()), ("include_users", "true".to_string())]).await.map_err(|e| anyhow!("{e}"))?;
    let mut list = unpack(handle, &answer);
    list.sort_by(|a, b| a["_id"].as_str().cmp(&b["_id"].as_str()));
    Ok(list.iter().filter(|m| messages::record(state, handle, m)).count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hit_is_a_row_with_its_author_named_and_its_time_from_its_id() {
        let h = crate::backend::stoat::tests::bare_handle("stoat:ME");
        {
            let mut w = h.world.lock().unwrap();
            w.own_id = "01ME000000000000000000000A".into();
            w.users.insert("01PAL00000000000000000000A".into(), super::super::world::User { name: "Pal".into(), ..Default::default() });
        }
        let buffer = "stoat:ME|general";
        let row = row(&h, buffer, &json!({ "_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "channel": "C", "author": "01PAL00000000000000000000A", "content": "hello there" })).unwrap();
        assert_eq!((row["from"].as_str(), row["body"].as_str(), row["ts"].as_i64(), row["isOwn"].as_bool()), (Some("Pal"), Some("hello there"), Some(1_469_922_850), Some(false)));
        let blank = super::row(&h, buffer, &json!({ "_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "author": "01ME000000000000000000000A", "content": "" })).unwrap();
        assert_eq!((blank["body"].as_str(), blank["isOwn"].as_bool()), (Some("(no text)"), Some(true)));
        assert!(super::row(&h, buffer, &json!({ "content": "no id" })).is_none());
    }

    #[test]
    fn an_answer_may_be_a_bare_list_or_an_object_that_carries_the_people() {
        let h = crate::backend::stoat::tests::bare_handle("stoat:ME");
        assert_eq!(unpack(&h, &json!([{ "_id": "A" }])).len(), 1);
        let list = unpack(&h, &json!({ "messages": [{ "_id": "A" }, { "_id": "B" }], "users": [{ "_id": "01NEW00000000000000000000A", "username": "newbie" }] }));
        assert_eq!(list.len(), 2);
        assert_eq!(h.world.lock().unwrap().name_of("01NEW00000000000000000000A"), "newbie");
    }
}
