//! Rocket.Chat server-side search (with E2EE local-scrollback fallback notice),
//! pinned messages, starred messages, room file list, and read receipts.

use super::world::RoomKind;
use super::Handle;
use crate::state::AppState;
use anyhow::Result;
use serde_json::{json, Value};

/// Search messages in a room (`GET /api/v1/chat.search`).
/// For E2EE rooms, server-side search cannot index ciphertext — returns a notice and falls back
/// to moho's local SQLite scrollback search.
pub async fn search_room(
    state: &AppState,
    handle: &Handle,
    buffer_id: &str,
    room_id: &str,
    query: &str,
    count: usize,
) -> Result<Value> {
    let encrypted = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rooms
        .get(room_id)
        .is_some_and(|r| r.encrypted);

    if encrypted {
        let local = state
            .store
            .search_messages(buffer_id, query, count as i64)
            .unwrap_or_default();
        return Ok(json!({
            "messages": local,
            "localFallback": true,
            "notice": "Server-side search is unavailable in end-to-end encrypted Rocket.Chat rooms. Showing local scrollback matches instead.",
        }));
    }

    Ok(handle
        .api
        .get(
            "/api/v1/chat.search",
            &[
                ("roomId", room_id.to_string()),
                ("searchText", query.to_string()),
                ("count", count.to_string()),
            ],
        )
        .await?)
}

/// Fetch pinned messages in a room (`GET /api/v1/chat.getPinnedMessages`).
pub async fn pinned_messages(handle: &Handle, room_id: &str, count: usize) -> Result<Value> {
    Ok(handle
        .api
        .get(
            "/api/v1/chat.getPinnedMessages",
            &[("roomId", room_id.to_string()), ("count", count.to_string())],
        )
        .await?)
}

/// Fetch starred messages in a room (`GET /api/v1/chat.getStarredMessages`).
pub async fn starred_messages(handle: &Handle, room_id: &str, count: usize) -> Result<Value> {
    Ok(handle
        .api
        .get(
            "/api/v1/chat.getStarredMessages",
            &[("roomId", room_id.to_string()), ("count", count.to_string())],
        )
        .await?)
}

/// List files in a room (`channels.files`, `groups.files`, `im.files`).
pub async fn room_files(handle: &Handle, room_id: &str, count: usize) -> Result<Value> {
    let kind = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rooms
        .get(room_id)
        .map(|r| r.kind)
        .unwrap_or(RoomKind::Channel);
    let path = format!("/api/v1/{}.files", kind.endpoint_prefix());
    Ok(handle
        .api
        .get(&path, &[("roomId", room_id.to_string()), ("count", count.to_string())])
        .await?)
}

/// Fetch read receipts for a message (`GET /api/v1/chat.getMessageReadReceipts`),
/// when the server has `Message_Read_Receipt_Enabled`.
pub async fn read_receipts(handle: &Handle, msg_id: &str) -> Result<Value> {
    if !handle.server.read_receipts_enabled {
        return Ok(json!({ "receipts": [], "enabled": false }));
    }
    Ok(handle
        .api
        .get("/api/v1/chat.getMessageReadReceipts", &[("messageId", msg_id.to_string())])
        .await?)
}
