//! Rocket.Chat room operations:
//! - Directory browse/search (`GET /api/v1/directory?type=channels|teams|users`)
//! - Open 1:1 and multi-user DMs (`POST /api/v1/im.create`)
//! - Create channels, private groups, teams, discussions
//! - Join / leave / favorite / hide rooms
//! - Room info, room settings (`rooms.saveRoomSettings`), archive/unarchive, delete, clean history
//! - Invite links (`findOrCreateInvite`, `listInvites`, `removeInvite`, `useInviteToken`)

use super::world::RoomKind;
use super::{connect, events, Handle};
use crate::state::AppState;
use anyhow::Result;
use serde_json::{json, Value};

/// Search public directory (`channels`, `teams`, or `users`).
pub async fn directory(
    handle: &Handle,
    kind: &str,
    query: &str,
    count: usize,
) -> Result<Value> {
    let q = json!({ "text": query, "type": kind }).to_string();
    Ok(handle
        .api
        .get(
            "/api/v1/directory",
            &[("query", q), ("count", count.to_string())],
        )
        .await?)
}

/// Open a 1:1 or multi-user DM (`POST /api/v1/im.create`).
pub async fn create_dm(
    state: &AppState,
    handle: &Handle,
    usernames: &[String],
) -> Result<Value> {
    let body = if usernames.len() == 1 {
        json!({ "username": usernames[0] })
    } else {
        json!({ "usernames": usernames.join(",") })
    };
    let resp = handle.api.post("/api/v1/im.create", &body).await?;
    if let Some(rid) = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .merge_room_json(&resp["room"])
    {
        if let Some(room) = handle
            .world
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rooms
            .get(&rid)
            .cloned()
        {
            events::register_room(state, handle, &room);
            connect::subscribe_room(handle, &rid);
        }
    }
    Ok(resp)
}

/// Join a public channel (`POST /api/v1/channels.join`).
pub async fn join_channel(state: &AppState, handle: &Handle, room_id: &str, join_code: Option<&str>) -> Result<Value> {
    let mut body = json!({ "roomId": room_id });
    if let Some(code) = join_code.filter(|s| !s.is_empty()) {
        body["joinCode"] = json!(code);
    }
    let resp = handle.api.post("/api/v1/channels.join", &body).await?;
    if let Some(rid) = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .merge_room_json(&resp["channel"])
    {
        if let Some(room) = handle
            .world
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rooms
            .get(&rid)
            .cloned()
        {
            events::register_room(state, handle, &room);
            connect::subscribe_room(handle, &rid);
        }
    }
    Ok(resp)
}

/// Leave a channel, private group, or team.
pub async fn leave_room(handle: &Handle, room_id: &str, team_id: Option<&str>) -> Result<Value> {
    if let Some(tid) = team_id.filter(|s| !s.is_empty()) {
        return Ok(handle.api.post("/api/v1/teams.leave", &json!({ "teamId": tid })).await?);
    }
    let kind = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rooms
        .get(room_id)
        .map(|r| r.kind)
        .unwrap_or(RoomKind::Channel);
    let path = match kind {
        RoomKind::Group => "/api/v1/groups.leave",
        _ => "/api/v1/channels.leave",
    };
    let resp = handle.api.post(path, &json!({ "roomId": room_id })).await?;
    handle.world.lock().unwrap_or_else(|e| e.into_inner()).rooms.remove(room_id);
    Ok(resp)
}

/// Create a public channel (`channels.create`) or private group (`groups.create`).
pub async fn create_channel_or_group(
    state: &AppState,
    handle: &Handle,
    name: &str,
    private: bool,
    members: &[String],
    read_only: bool,
    encrypted: bool,
) -> Result<Value> {
    let path = if private { "/api/v1/groups.create" } else { "/api/v1/channels.create" };
    let body = json!({
        "name": name,
        "members": members,
        "readOnly": read_only,
        "extraData": { "encrypted": encrypted },
    });
    let resp = handle.api.post(path, &body).await?;
    let key = if private { "group" } else { "channel" };
    if let Some(rid) = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .merge_room_json(&resp[key])
    {
        if let Some(room) = handle
            .world
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rooms
            .get(&rid)
            .cloned()
        {
            events::register_room(state, handle, &room);
            connect::subscribe_room(handle, &rid);
        }
    }
    Ok(resp)
}

/// Create a Team (`POST /api/v1/teams.create`). `team_type`: 0 = public, 1 = private.
pub async fn create_team(
    state: &AppState,
    handle: &Handle,
    name: &str,
    private: bool,
    members: &[String],
) -> Result<Value> {
    let resp = handle
        .api
        .post(
            "/api/v1/teams.create",
            &json!({
                "name": name,
                "type": if private { 1 } else { 0 },
                "members": members,
            }),
        )
        .await?;
    if let Some(rid) = resp["team"]["roomId"].as_str() {
        let _ = refresh_room_info(state, handle, rid).await;
    }
    Ok(resp)
}

/// List teams (`GET /api/v1/teams.list`).
pub async fn list_teams(handle: &Handle) -> Result<Value> {
    Ok(handle.api.get("/api/v1/teams.list", &[]).await?)
}

/// Create a Discussion (`POST /api/v1/rooms.createDiscussion`).
pub async fn create_discussion(
    state: &AppState,
    handle: &Handle,
    parent_room_id: &str,
    name: &str,
    reply: Option<&str>,
    users: &[String],
) -> Result<Value> {
    let mut body = json!({
        "prid": parent_room_id,
        "t_name": name,
        "users": users,
    });
    if let Some(r) = reply.filter(|s| !s.is_empty()) {
        body["reply"] = json!(r);
    }
    let resp = handle.api.post("/api/v1/rooms.createDiscussion", &body).await?;
    if let Some(rid) = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .merge_room_json(&resp["discussion"])
    {
        if let Some(room) = handle
            .world
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rooms
            .get(&rid)
            .cloned()
        {
            events::register_room(state, handle, &room);
            connect::subscribe_room(handle, &rid);
        }
    }
    Ok(resp)
}

/// List discussions in a room (`GET /api/v1/rooms.getDiscussions`).
pub async fn list_discussions(handle: &Handle, room_id: &str) -> Result<Value> {
    Ok(handle
        .api
        .get("/api/v1/rooms.getDiscussions", &[("roomId", room_id.to_string())])
        .await?)
}

/// Fetch and merge `GET /api/v1/rooms.info`.
pub async fn refresh_room_info(state: &AppState, handle: &Handle, room_id: &str) -> Result<Value> {
    let resp = handle
        .api
        .get("/api/v1/rooms.info", &[("roomId", room_id.to_string())])
        .await?;
    if let Some(rid) = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .merge_room_json(&resp["room"])
    {
        if let Some(room) = handle
            .world
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rooms
            .get(&rid)
            .cloned()
        {
            events::register_room(state, handle, &room);
        }
    }
    Ok(resp)
}

/// Favorite or unfavorite a room (`POST /api/v1/rooms.favorite`).
pub async fn set_favorite(handle: &Handle, room_id: &str, favorite: bool) -> Result<Value> {
    let resp = handle
        .api
        .post("/api/v1/rooms.favorite", &json!({ "roomId": room_id, "favorite": favorite }))
        .await?;
    if let Some(r) = handle.world.lock().unwrap_or_else(|e| e.into_inner()).rooms.get_mut(room_id) {
        r.favorite = favorite;
    }
    Ok(resp)
}

/// Hide / close a room from sidebar (`im.close`, `channels.close`, `groups.close`).
pub async fn hide_room(handle: &Handle, room_id: &str) -> Result<Value> {
    let kind = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rooms
        .get(room_id)
        .map(|r| r.kind)
        .unwrap_or(RoomKind::Channel);
    let path = format!("/api/v1/{}.close", kind.endpoint_prefix());
    let resp = handle.api.post(&path, &json!({ "roomId": room_id })).await?;
    if let Some(r) = handle.world.lock().unwrap_or_else(|e| e.into_inner()).rooms.get_mut(room_id) {
        r.open = false;
    }
    Ok(resp)
}

/// Save room settings (`POST /api/v1/rooms.saveRoomSettings`).
/// Supports name, topic, announcement, description, readOnly, reactWhenReadOnly,
/// roomType (`c`/`p`), encrypted, broadcast.
pub async fn save_room_settings(handle: &Handle, room_id: &str, settings: Value) -> Result<Value> {
    let mut payload = settings;
    payload["rid"] = json!(room_id);
    Ok(handle.api.post("/api/v1/rooms.saveRoomSettings", &payload).await?)
}

/// Archive or unarchive a channel or private group.
pub async fn set_archived(handle: &Handle, room_id: &str, archived: bool) -> Result<Value> {
    let kind = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rooms
        .get(room_id)
        .map(|r| r.kind)
        .unwrap_or(RoomKind::Channel);
    let action = if archived { "archive" } else { "unarchive" };
    let path = format!("/api/v1/{}.{action}", kind.endpoint_prefix());
    Ok(handle.api.post(&path, &json!({ "roomId": room_id })).await?)
}

/// Delete a channel or private group.
pub async fn delete_room(handle: &Handle, room_id: &str) -> Result<Value> {
    let kind = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rooms
        .get(room_id)
        .map(|r| r.kind)
        .unwrap_or(RoomKind::Channel);
    let path = format!("/api/v1/{}.delete", kind.endpoint_prefix());
    let resp = handle.api.post(&path, &json!({ "roomId": room_id })).await?;
    handle.world.lock().unwrap_or_else(|e| e.into_inner()).rooms.remove(room_id);
    Ok(resp)
}

/// Prune messages in a room (`POST /api/v1/rooms.cleanHistory`).
pub async fn clean_history(
    handle: &Handle,
    room_id: &str,
    latest_iso: &str,
    oldest_iso: &str,
    inclusive: bool,
    exclude_pinned: bool,
    files_only: bool,
) -> Result<Value> {
    Ok(handle
        .api
        .post(
            "/api/v1/rooms.cleanHistory",
            &json!({
                "roomId": room_id,
                "latest": latest_iso,
                "oldest": oldest_iso,
                "inclusive": inclusive,
                "excludePinned": exclude_pinned,
                "filesOnly": files_only,
            }),
        )
        .await?)
}

/// Create or reuse a room invite link (`POST /api/v1/findOrCreateInvite`).
pub async fn create_invite(handle: &Handle, room_id: &str, days: u32, max_uses: u32) -> Result<Value> {
    Ok(handle
        .api
        .post(
            "/api/v1/findOrCreateInvite",
            &json!({ "rid": room_id, "days": days, "maxUses": max_uses }),
        )
        .await?)
}

/// List invites (`GET /api/v1/listInvites`).
pub async fn list_invites(handle: &Handle) -> Result<Value> {
    Ok(handle.api.get("/api/v1/listInvites", &[]).await?)
}

/// Revoke an invite (`DELETE /api/v1/removeInvite/{id}`).
pub async fn remove_invite(handle: &Handle, invite_id: &str) -> Result<Value> {
    Ok(handle
        .api
        .delete(&format!("/api/v1/removeInvite/{invite_id}"), &[])
        .await?)
}

/// Redeem a room invite token (`POST /api/v1/useInviteToken`).
pub async fn use_invite_token(handle: &Handle, token: &str) -> Result<Value> {
    Ok(handle
        .api
        .post("/api/v1/useInviteToken", &json!({ "token": token }))
        .await?)
}
