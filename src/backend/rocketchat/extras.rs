//! Round 8 Protocol-Specific Rocket.Chat Features:
//! - Slash commands (`commands.list`, `commands.run`, `commands.preview`)
//! - Omnichannel / Livechat (`livechat/rooms`, `livechat/room.close`) for `livechat-agent` accounts
//! - Matrix Federation badge helpers & OTR compatibility notices

use super::Handle;
use anyhow::Result;
use serde_json::{json, Value};

/// List available server slash commands (`GET /api/v1/commands.list`).
pub async fn list_commands(handle: &Handle) -> Result<Value> {
    Ok(handle.api.get("/api/v1/commands.list", &[]).await?)
}

/// Execute a server slash command (`POST /api/v1/commands.run`), e.g. `/shrug`, `/topic`, `/invite`.
pub async fn run_command(
    handle: &Handle,
    room_id: &str,
    command: &str,
    params: &str,
    thread_id: Option<&str>,
) -> Result<Value> {
    let mut body = json!({
        "command": command.trim_start_matches('/'),
        "roomId": room_id,
        "params": params,
    });
    if let Some(tmid) = thread_id.filter(|s| !s.is_empty()) {
        body["tmid"] = json!(tmid);
    }
    Ok(handle.api.post("/api/v1/commands.run", &body).await?)
}

/// Fetch slash command preview (`GET /api/v1/commands.preview`).
pub async fn preview_command(handle: &Handle, room_id: &str, command: &str, params: &str) -> Result<Value> {
    Ok(handle
        .api
        .get(
            "/api/v1/commands.preview",
            &[
                ("command", command.trim_start_matches('/').to_string()),
                ("roomId", room_id.to_string()),
                ("params", params.to_string()),
            ],
        )
        .await?)
}

/// List Omnichannel / Livechat rooms (`GET /api/v1/livechat/rooms`).
pub async fn list_livechat_rooms(handle: &Handle, open_only: bool) -> Result<Value> {
    Ok(handle
        .api
        .get(
            "/api/v1/livechat/rooms",
            &[("open", open_only.to_string())],
        )
        .await?)
}

/// Close an Omnichannel / Livechat room (`POST /api/v1/livechat/room.close`).
pub async fn close_livechat_room(handle: &Handle, room_id: &str, token: &str, _comment: Option<&str>) -> Result<Value> {
    let body = json!({ "rid": room_id, "token": token });
    Ok(handle.api.post("/api/v1/livechat/room.close", &body).await?)
}
