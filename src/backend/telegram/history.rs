//! A conversation's history, read when it is opened and again when it is scrolled back.
//!
//! Telegram keeps every message, so opening a conversation asks for its recent end, and scrolling to the top asks for
//! the part before. What is stored is returned by `getBacklog` either way, so a failure to reach Telegram leaves what
//! there is rather than an error.

use super::peers;
use super::topics;
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use grammers_session::Session;

/// How many messages a page asks for.
const PAGE: usize = 50;

/// A page of one topic's messages, as the messages themselves. Telegram lists a topic's messages as raw ones, so the
/// ids are taken from that answer and the messages asked for by id: the library hands those over whole.
async fn topic_page(
    handle: &super::Handle,
    peer: grammers_session::types::PeerRef,
    topic: i32,
    offset_id: i32,
    offset_date: i32,
    add_offset: i32,
    limit: i32,
) -> Result<Vec<grammers_client::message::Message>> {
    use grammers_client::tl;
    let answer = handle
        .client
        .invoke(&tl::functions::messages::GetReplies { peer: peer.into(), msg_id: topic, offset_id, offset_date, add_offset, limit, max_id: 0, min_id: 0, hash: 0 })
        .await
        .map_err(|e| anyhow!("{}", super::send::explain(&e)))?;
    let listed = match answer {
        tl::enums::messages::Messages::Messages(m) => m.messages,
        tl::enums::messages::Messages::Slice(m) => m.messages,
        tl::enums::messages::Messages::ChannelMessages(m) => m.messages,
        tl::enums::messages::Messages::NotModified(_) => Vec::new(),
    };
    let mut ids: Vec<i32> = listed
        .iter()
        .filter_map(|m| match m {
            tl::enums::Message::Message(m) => Some(m.id),
            tl::enums::Message::Service(m) => Some(m.id),
            tl::enums::Message::Empty(_) => None,
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let found = handle.client.get_messages_by_id(peer, &ids).await.map_err(|e| anyhow!("{}", super::send::explain(&e)))?;
    Ok(found.into_iter().flatten().collect())
}

async fn read(state: &AppState, buffer: &Buffer, before: Option<i64>) -> Result<usize> {
    let handle = state.runtime.telegram_handle(&buffer.account_id).ok_or_else(|| anyhow!("this Telegram account is not connected"))?;
    let peer = buffer.remote_id.as_deref().and_then(peers::peer_id_of).ok_or_else(|| anyhow!("this is not a Telegram conversation"))?;
    let peer = handle.session.peer_ref(peer).await.ok().flatten().ok_or_else(|| anyhow!("Telegram has not told this account about that conversation yet"))?;
    if let Some(topic) = topics::topic_of_buffer(buffer) {
        let date = before.filter(|t| *t > 0).map(|t| t.clamp(0, i64::from(i32::MAX)) as i32).unwrap_or(0);
        let got = topic_page(&handle, peer, topic, 0, date, 0, PAGE as i32).await?;
        let count = got.len();
        for message in &got {
            super::messages::record(state, &buffer.account_id, &handle.own, message);
        }
        return Ok(count);
    }
    let mut iter = handle.client.iter_messages(peer).limit(PAGE);
    if let Some(ts) = before.filter(|t| *t > 0) {
        iter = iter.offset_date(ts.clamp(0, i64::from(i32::MAX)) as i32);
    }
    let mut got = Vec::new();
    while let Some(message) = iter.next().await.map_err(|e| anyhow!("{e}"))? {
        got.push(message);
    }
    // Oldest first, so a reply finds the message it answers already stored.
    got.reverse();
    let count = got.len();
    for message in &got {
        super::messages::record(state, &buffer.account_id, &handle.own, message);
    }
    Ok(count)
}

/// Opening a conversation: its recent end.
pub async fn open(state: &AppState, buffer: &Buffer) -> Result<usize> {
    read(state, buffer, None).await
}

/// Scrolling back: what came before `before` (a Unix time).
pub async fn older(state: &AppState, buffer: &Buffer, before: i64) -> Result<usize> {
    read(state, buffer, Some(before)).await
}

/// Reads the messages around one, so a jump to an old message (a pin, a reply, a search result) arrives with what was
/// said before and after it. Returns the message's time.
pub async fn context(state: &AppState, buffer: &Buffer, msg_id: &str) -> Result<i64> {
    let id: i32 = msg_id.parse().map_err(|_| anyhow!("that is not a Telegram message"))?;
    let handle = state.runtime.telegram_handle(&buffer.account_id).ok_or_else(|| anyhow!("this Telegram account is not connected"))?;
    let peer = buffer.remote_id.as_deref().and_then(peers::peer_id_of).ok_or_else(|| anyhow!("this is not a Telegram conversation"))?;
    let peer = handle.session.peer_ref(peer).await.ok().flatten().ok_or_else(|| anyhow!("Telegram has not told this account about that conversation yet"))?;
    if let Some(topic) = topics::topic_of_buffer(buffer) {
        let mut around = topic_page(&handle, peer, topic, id.saturating_add(1), 0, 0, 26).await?;
        around.extend(topic_page(&handle, peer, topic, id, 0, -25, 25).await?);
        around.sort_by_key(|m| m.id());
        around.dedup_by_key(|m| m.id());
        for message in &around {
            super::messages::record(state, &buffer.account_id, &handle.own, message);
        }
        return around.iter().find(|m| m.id() == id).map(|m| m.date().as_second()).ok_or_else(|| anyhow!("that message is no longer there"));
    }
    let mut around = Vec::new();
    // Itself and what came before it, newest first...
    let mut older = handle.client.iter_messages(peer).offset_id(id.saturating_add(1)).limit(26);
    while let Some(message) = older.next().await.map_err(|e| anyhow!("{e}"))? {
        around.push(message);
    }
    // ...and what came after, oldest first.
    let mut newer = handle.client.iter_messages(peer).reverse(true).offset_id(id).limit(25);
    while let Some(message) = newer.next().await.map_err(|e| anyhow!("{e}"))? {
        around.push(message);
    }
    around.sort_by_key(|m| m.id());
    for message in &around {
        super::messages::record(state, &buffer.account_id, &handle.own, message);
    }
    around.iter().find(|m| m.id() == id).map(|m| m.date().as_second()).ok_or_else(|| anyhow!("that message is no longer there"))
}

/// Reads on from a message towards the present, for a reader who arrived in the middle of a conversation.
pub async fn newer(state: &AppState, buffer: &Buffer, msg_id: &str) -> Result<usize> {
    let id: i32 = msg_id.parse().map_err(|_| anyhow!("that is not a Telegram message"))?;
    let handle = state.runtime.telegram_handle(&buffer.account_id).ok_or_else(|| anyhow!("this Telegram account is not connected"))?;
    let peer = buffer.remote_id.as_deref().and_then(peers::peer_id_of).ok_or_else(|| anyhow!("this is not a Telegram conversation"))?;
    let peer = handle.session.peer_ref(peer).await.ok().flatten().ok_or_else(|| anyhow!("Telegram has not told this account about that conversation yet"))?;
    if let Some(topic) = topics::topic_of_buffer(buffer) {
        let got = topic_page(&handle, peer, topic, id, 0, -(PAGE as i32), PAGE as i32).await?;
        for message in got.iter().filter(|m| m.id() > id) {
            super::messages::record(state, &buffer.account_id, &handle.own, message);
        }
        return Ok(got.iter().filter(|m| m.id() > id).count());
    }
    let mut iter = handle.client.iter_messages(peer).reverse(true).offset_id(id).limit(PAGE);
    let mut count = 0;
    while let Some(message) = iter.next().await.map_err(|e| anyhow!("{e}"))? {
        super::messages::record(state, &buffer.account_id, &handle.own, &message);
        count += 1;
    }
    Ok(count)
}
