//! Who is in a group: the list beside the conversation.
//!
//! Telegram will list the members of a group or a supergroup (a channel's subscribers are for its administrators
//! only), two hundred at a time. The first page is read when somebody opens the conversation, with the group's own
//! count, and the owner and administrators carry a mark in front of their name.

use super::media;
use super::messages;
use super::send::{explain, peer_for};
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::peer::Role;
use serde_json::{json, Value};

/// How many members are read: Telegram's own page.
const LIMIT: usize = 200;

/// What stands in front of an owner's or an administrator's name.
const OWNER: &str = "👑";
const ADMIN: &str = "@";

/// Reads a group's members and says so, as the list and as how many there are in all.
pub async fn refresh(state: &AppState, buffer: &Buffer) -> Result<usize, String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    // A person's conversation, a channel somebody only follows, or the account's own: nobody to list.
    if buffer.kind == "dm" || buffer.category.as_deref() == Some(super::peers::CHANNELS) {
        return Ok(0);
    }
    let mut iter = handle.client.iter_participants(peer);
    let total = iter.total().await.unwrap_or(0);
    let mut members: Vec<Value> = Vec::new();
    while members.len() < LIMIT {
        let Some(found) = iter.next().await.map_err(|e| explain(&e))? else { break };
        let (prefix, banned) = match &found.role {
            Role::Creator(_) => (OWNER, false),
            Role::Admin(_) => (ADMIN, false),
            Role::Banned(_) | Role::Left(_) => ("", true),
            _ => ("", false),
        };
        if banned {
            continue;
        }
        let peer = grammers_client::peer::Peer::User(found.user.clone());
        handle.names.lock().unwrap_or_else(|e| e.into_inner()).insert(found.user.id().bare_id_unchecked(), messages::name_of(&peer));
        members.push(json!({
            "nick": messages::name_of(&peer),
            "userId": found.user.id().bare_id_unchecked().to_string(),
            "prefix": prefix,
            "bot": found.user.is_bot(),
            "avatarUrl": media::avatar_url(handle.own.user_id, &peer),
        }));
    }
    let count = members.len();
    let list = Value::Array(members);
    state.runtime.set_presence(&buffer.id, list.clone());
    state.runtime.set_presence_total(&buffer.id, total.max(count));
    state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": list, "total": total.max(count) }));
    Ok(count)
}
