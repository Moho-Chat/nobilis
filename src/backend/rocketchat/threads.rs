//! Rocket.Chat Threads: fetch thread messages, list room threads, follow/unfollow thread.

use super::Handle;
use anyhow::Result;
use serde_json::{json, Value};

/// Fetch messages in a thread (`GET /api/v1/chat.getThreadMessages`).
pub async fn get_thread_messages(handle: &Handle, tmid: &str, count: usize) -> Result<Value> {
    Ok(handle
        .api
        .get(
            "/api/v1/chat.getThreadMessages",
            &[("tmid", tmid.to_string()), ("count", count.to_string())],
        )
        .await?)
}

/// List threads in a room (`GET /api/v1/chat.getThreadsList`).
pub async fn get_threads_list(handle: &Handle, room_id: &str, count: usize) -> Result<Value> {
    Ok(handle
        .api
        .get(
            "/api/v1/chat.getThreadsList",
            &[("rid", room_id.to_string()), ("count", count.to_string())],
        )
        .await?)
}

/// Follow or unfollow a thread (`POST /api/v1/chat.followMessage` / `chat.unfollowMessage`).
pub async fn set_following(handle: &Handle, msg_id: &str, follow: bool) -> Result<Value> {
    let path = if follow {
        "/api/v1/chat.followMessage"
    } else {
        "/api/v1/chat.unfollowMessage"
    };
    Ok(handle.api.post(path, &json!({ "mid": msg_id })).await?)
}
