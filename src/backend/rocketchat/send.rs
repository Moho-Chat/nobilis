//! Outgoing Rocket.Chat actions: send text / thread reply / quoted reply (plaintext or E2EE),
//! edit, delete, react, typing indicator, read/unread sync, pin/unpin, star/unstar.

use super::ddp;
use super::world::RoomKind;
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// Send a message to a Rocket.Chat room.
/// - If `thread_id` (`tmid`) is set, sends as a thread reply (optionally also shown in channel with `tshow`).
/// - If `quote_id` is set, prepends Rocket.Chat's quoted permalink `[ ](https://<host>/...)`.
/// - If the room is E2EE-enabled and unlocked, encrypts via `e2ee::encrypt_message`.
pub async fn send_message(
    state: &AppState,
    handle: &Handle,
    buffer: &Buffer,
    text: &str,
    thread_id: Option<&str>,
    tshow: bool,
    quote_id: Option<&str>,
) -> Result<Value> {
    let rid = buffer
        .remote_id
        .as_deref()
        .ok_or_else(|| anyhow!("Rocket.Chat buffer missing room id"))?;

    let (is_encrypted, room_path) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let room = world.rooms.get(rid);
        let enc = room.is_some_and(|r| r.encrypted);
        let path = match room.map(|r| (r.kind, r.name.as_str())) {
            Some((RoomKind::Direct, _)) => format!("direct/{rid}"),
            Some((RoomKind::Group, name)) => format!("group/{name}"),
            Some((_, name)) => format!("channel/{name}"),
            None => format!("channel/{rid}"),
        };
        (enc, path)
    };

    let final_text = if let Some(qid) = quote_id.filter(|s| !s.is_empty()) {
        format!("[ ]({}/{room_path}?msg={qid}) {text}", handle.api.base())
    } else {
        text.to_string()
    };

    let resp = if is_encrypted {
        let mut message = super::e2ee::build_encrypted_message_payload(handle, rid, &final_text)?;
        if let Some(tmid) = thread_id.filter(|s| !s.is_empty()) {
            message["tmid"] = json!(tmid);
            if tshow {
                message["tshow"] = json!(true);
            }
        }
        handle
            .api
            .post("/api/v1/chat.sendMessage", &json!({ "message": message }))
            .await?
    } else {
        // `chat.sendMessage` supports `tmid` + `tshow` consistently across Rocket.Chat v6/v7/v8/v9.
        let mut msg = json!({
            "rid": rid,
            "msg": final_text,
        });
        if let Some(tmid) = thread_id.filter(|s| !s.is_empty()) {
            msg["tmid"] = json!(tmid);
            if tshow {
                msg["tshow"] = json!(true);
            }
        }
        handle
            .api
            .post("/api/v1/chat.sendMessage", &json!({ "message": msg }))
            .await?
    };

    if let Some(msg) = resp.get("message") {
        super::messages::record(state, handle, msg);
    }
    Ok(resp)
}

/// Edit an existing message (`POST /api/v1/chat.update`).
pub async fn edit_message(
    state: &AppState,
    handle: &Handle,
    buffer: &Buffer,
    msg_id: &str,
    new_text: &str,
) -> Result<Value> {
    let rid = buffer
        .remote_id
        .as_deref()
        .ok_or_else(|| anyhow!("Rocket.Chat buffer missing room id"))?;
    let resp = handle
        .api
        .post(
            "/api/v1/chat.update",
            &json!({ "roomId": rid, "msgId": msg_id, "text": new_text }),
        )
        .await?;
    state.runtime.update_message_body_only(state, &buffer.id, msg_id, new_text);
    Ok(resp)
}

/// Delete a message (`POST /api/v1/chat.delete`).
pub async fn delete_message(
    state: &AppState,
    handle: &Handle,
    buffer: &Buffer,
    msg_id: &str,
) -> Result<Value> {
    let rid = buffer
        .remote_id
        .as_deref()
        .ok_or_else(|| anyhow!("Rocket.Chat buffer missing room id"))?;
    let resp = handle
        .api
        .post(
            "/api/v1/chat.delete",
            &json!({ "roomId": rid, "msgId": msg_id, "asUser": true }),
        )
        .await?;
    state.runtime.delete_message(state, &buffer.id, msg_id);
    Ok(resp)
}

/// Toggle a reaction on a message (`POST /api/v1/chat.react`).
pub async fn react(
    handle: &Handle,
    msg_id: &str,
    emoji: &str,
    should_react: bool,
) -> Result<Value> {
    let resp = handle
        .api
        .post(
            "/api/v1/chat.react",
            &json!({
                "messageId": msg_id,
                "emoji": emoji,
                "shouldReact": should_react,
            }),
        )
        .await?;
    Ok(resp)
}

/// Send typing indicator via DDP `stream-notify-room` (`{rid}/user-activity`).
pub fn send_typing(handle: &Handle, buffer: &Buffer, is_typing: bool) -> Result<()> {
    let rid = buffer
        .remote_id
        .as_deref()
        .ok_or_else(|| anyhow!("Rocket.Chat buffer missing room id"))?;
    let username = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .own_username
        .clone();
    let id = ddp::next_id("typing");
    handle
        .ddp
        .send(ddp::typing_frame(&id, rid, &username, is_typing))
        .map_err(|e| anyhow!("DDP typing send failed: {e}"))
}

/// Mark room as read (`POST /api/v1/subscriptions.read`).
pub async fn mark_read(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<()> {
    let Some(rid) = buffer.remote_id.as_deref() else {
        return Ok(());
    };
    handle
        .api
        .post("/api/v1/subscriptions.read", &json!({ "rid": rid }))
        .await?;
    state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
    Ok(())
}

/// Mark room as unread (`POST /api/v1/subscriptions.unread`).
pub async fn mark_unread(state: &AppState, handle: &Handle, buffer: &Buffer, first_unread_msg: Option<&str>) -> Result<()> {
    let rid = buffer
        .remote_id
        .as_deref()
        .ok_or_else(|| anyhow!("Rocket.Chat buffer missing room id"))?;
    let body = match first_unread_msg {
        Some(mid) => json!({ "firstUnreadMessage": { "_id": mid } }),
        None => json!({ "roomId": rid }),
    };
    handle.api.post("/api/v1/subscriptions.unread", &body).await?;
    state.runtime.set_buffer_unread(state, &buffer.id, 1, 0);
    Ok(())
}

/// Pin or unpin a message (`POST /api/v1/chat.pinMessage` / `chat.unPinMessage`).
pub async fn set_pinned(handle: &Handle, msg_id: &str, pinned: bool) -> Result<Value> {
    let path = if pinned {
        "/api/v1/chat.pinMessage"
    } else {
        "/api/v1/chat.unPinMessage"
    };
    Ok(handle.api.post(path, &json!({ "messageId": msg_id })).await?)
}

/// Star or unstar a message (`POST /api/v1/chat.starMessage` / `chat.unStarMessage`).
pub async fn set_starred(handle: &Handle, msg_id: &str, starred: bool) -> Result<Value> {
    let path = if starred {
        "/api/v1/chat.starMessage"
    } else {
        "/api/v1/chat.unStarMessage"
    };
    Ok(handle.api.post(path, &json!({ "messageId": msg_id })).await?)
}
