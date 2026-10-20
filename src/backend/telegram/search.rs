//! Searching on Telegram's side: in one conversation or in every one the account has.
//!
//! The window searches its own copy first and shows that at once; this adds what the account's history has that this
//! client never read. A hit is answered as a message, and nothing is recorded: pressing one reads the messages around
//! it (see `history::context`) the way a jump to anything old does.

use super::media;
use super::messages;
use super::send::{explain, peer_for};
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::message::Message;
use serde_json::{json, Value};

/// One hit as the window reads a message.
fn hit(state: &AppState, account_id: &str, own_name: &str, own_id: i64, message: &Message) -> Option<Value> {
    let peer = message.peer()?;
    let buffer = super::peers::buffer_for(state, account_id, peer);
    let from = if message.outgoing() { own_name.to_string() } else { message.sender().map(messages::name_of).unwrap_or_else(|| super::peers::title_of(peer)) };
    let remote = buffer.remote_id.clone().unwrap_or_default();
    Some(json!({
        "id": message.id().to_string(),
        "bufferId": buffer.id,
        "from": from,
        "body": messages::body_of(message),
        "ts": message.date().as_second(),
        "kind": "chat",
        "isAction": false,
        "isHighlight": false,
        "edited": false,
        "isOwn": message.outgoing(),
        "attachments": media::attachments_of(own_id, &remote, message),
    }))
}

/// The matches for a query, in this conversation or across the account (`account` scope).
pub async fn run(state: &AppState, buffer: &Buffer, query: &str, account_wide: bool, limit: usize) -> Result<Value, String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    let mut results: Vec<Value> = Vec::new();
    if account_wide {
        let mut found = handle.client.search_all_messages().query(query);
        while results.len() < limit {
            let Some(message) = found.next().await.map_err(|e| explain(&e))? else { break };
            results.extend(hit(state, &buffer.account_id, &handle.own.name, handle.own.user_id, &message));
        }
    } else {
        let mut found = handle.client.search_messages(peer).query(query);
        while results.len() < limit {
            let Some(message) = found.next().await.map_err(|e| explain(&e))? else { break };
            results.extend(hit(state, &buffer.account_id, &handle.own.name, handle.own.user_id, &message));
        }
    }
    let names: std::collections::HashMap<String, String> = state.runtime.list_buffers().into_iter().filter(|b| b.account_id == buffer.account_id).map(|b| (b.id, b.name)).collect();
    let count = results.len();
    Ok(json!({ "results": results, "roomNames": names, "count": count }))
}
