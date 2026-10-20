//! A conversation's pinned messages: listed, pinned and unpinned.
//!
//! Telegram keeps the list on its side, so it is asked for when somebody opens the pins and again when an update says
//! the pins changed. The answer has the shape the other services' pins have, so the window's banner and list work
//! the same.

use super::messages;
use super::send::{explain, peer_for};
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::tl;
use serde_json::{json, Value};

/// How many pins are read: a conversation with more has more than anybody is going to scroll.
const LIMIT: usize = 50;

pub async fn list(state: &AppState, buffer: &Buffer) -> Result<Value, String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    let mut found = handle.client.search_messages(peer).filter(tl::enums::MessagesFilter::InputMessagesFilterPinned);
    let mut pinned: Vec<Value> = Vec::new();
    while pinned.len() < LIMIT {
        let Some(message) = found.next().await.map_err(|e| explain(&e))? else { break };
        let from = if message.outgoing() { handle.own.name.clone() } else { message.sender().map(messages::name_of).unwrap_or_else(|| buffer.name.clone()) };
        let body = messages::body_of(&message);
        pinned.push(json!({
            "id": message.id().to_string(),
            "bufferId": buffer.id,
            "from": from,
            "body": if body.trim().is_empty() { "(no text)".to_string() } else { body },
            "ts": message.date().as_second(),
            "kind": "chat",
        }));
    }
    let ids: Vec<String> = pinned.iter().filter_map(|m| m["id"].as_str().map(str::to_string)).collect();
    // Said as well as answered, so the header's count is right in every window watching this conversation.
    state.events.emit("pinnedMessages", json!({ "bufferId": buffer.id, "pinned": ids }));
    Ok(json!({ "bufferId": buffer.id, "pinned": pinned }))
}

pub async fn set(state: &AppState, buffer: &Buffer, msg_id: &str, pinned: bool) -> Result<(), String> {
    let id: i32 = msg_id.parse().map_err(|_| "that message cannot be pinned".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    if pinned {
        handle.client.pin_message(peer, id).await.map_err(|e| explain(&e))?;
    } else {
        handle.client.unpin_message(peer, id).await.map_err(|e| explain(&e))?;
    }
    Ok(())
}
