//! Rocket.Chat member list, invite/kick, mute/unmute, roles (owner/moderator/leader), and user ignores.

use super::world::RoomKind;
use super::Handle;
use anyhow::Result;
use serde_json::{json, Value};

/// Fetch room members (`channels.members`, `groups.members`, or `teams.members`).
pub async fn list_members(handle: &Handle, room_id: &str, team_id: Option<&str>, count: usize) -> Result<Value> {
    if let Some(tid) = team_id.filter(|s| !s.is_empty()) {
        return Ok(handle
            .api
            .get("/api/v1/teams.members", &[("teamId", tid.to_string()), ("count", count.to_string())])
            .await?);
    }
    let kind = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rooms
        .get(room_id)
        .map(|r| r.kind)
        .unwrap_or(RoomKind::Channel);
    let path = format!("/api/v1/{}.members", kind.endpoint_prefix());
    Ok(handle
        .api
        .get(&path, &[("roomId", room_id.to_string()), ("count", count.to_string())])
        .await?)
}

/// Invite a user to a channel, private group, or team.
pub async fn invite_member(handle: &Handle, room_id: &str, user_id: &str, team_id: Option<&str>) -> Result<Value> {
    if let Some(tid) = team_id.filter(|s| !s.is_empty()) {
        return Ok(handle
            .api
            .post(
                "/api/v1/teams.addMembers",
                &json!({ "teamId": tid, "members": [{ "userId": user_id, "roles": ["member"] }] }),
            )
            .await?);
    }
    let kind = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rooms
        .get(room_id)
        .map(|r| r.kind)
        .unwrap_or(RoomKind::Channel);
    let path = format!("/api/v1/{}.invite", kind.endpoint_prefix());
    Ok(handle
        .api
        .post(&path, &json!({ "roomId": room_id, "userId": user_id }))
        .await?)
}

/// Kick / remove a user from a channel, private group, or team.
pub async fn kick_member(handle: &Handle, room_id: &str, user_id: &str, team_id: Option<&str>) -> Result<Value> {
    if let Some(tid) = team_id.filter(|s| !s.is_empty()) {
        return Ok(handle
            .api
            .post("/api/v1/teams.removeMember", &json!({ "teamId": tid, "userId": user_id }))
            .await?);
    }
    let kind = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rooms
        .get(room_id)
        .map(|r| r.kind)
        .unwrap_or(RoomKind::Channel);
    let path = format!("/api/v1/{}.kick", kind.endpoint_prefix());
    Ok(handle
        .api
        .post(&path, &json!({ "roomId": room_id, "userId": user_id }))
        .await?)
}

/// Mute or unmute a user in a room (`POST /api/v1/rooms.muteUser` / `rooms.unmuteUser`).
pub async fn set_muted(handle: &Handle, room_id: &str, username: &str, muted: bool) -> Result<Value> {
    let path = if muted {
        "/api/v1/rooms.muteUser"
    } else {
        "/api/v1/rooms.unmuteUser"
    };
    Ok(handle
        .api
        .post(path, &json!({ "roomId": room_id, "username": username }))
        .await?)
}

/// Add or remove a room role (`owner`, `moderator`, `leader`) for a user in a channel/group/team.
pub async fn set_role(
    handle: &Handle,
    room_id: &str,
    user_id: &str,
    role: &str,
    add: bool,
    team_id: Option<&str>,
) -> Result<Value> {
    if let Some(tid) = team_id.filter(|s| !s.is_empty()) {
        let roles = if add { vec![role] } else { vec!["member"] };
        return Ok(handle
            .api
            .post(
                "/api/v1/teams.updateMember",
                &json!({ "teamId": tid, "member": { "userId": user_id, "roles": roles } }),
            )
            .await?);
    }
    let kind = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rooms
        .get(room_id)
        .map(|r| r.kind)
        .unwrap_or(RoomKind::Channel);
    let action = match (role, add) {
        ("owner", true) => "addOwner",
        ("owner", false) => "removeOwner",
        ("moderator", true) => "addModerator",
        ("moderator", false) => "removeModerator",
        ("leader", true) => "addLeader",
        ("leader", false) => "removeLeader",
        _ => anyhow::bail!("unsupported Rocket.Chat room role: {role}"),
    };
    let path = format!("/api/v1/{}.{action}", kind.endpoint_prefix());
    Ok(handle
        .api
        .post(&path, &json!({ "roomId": room_id, "userId": user_id }))
        .await?)
}

/// Ignore or unignore a user in a room (`GET /api/v1/chat.ignoreUser`).
pub async fn ignore_user(handle: &Handle, room_id: &str, user_id: &str, ignore: bool) -> Result<Value> {
    let resp = handle
        .api
        .get(
            "/api/v1/chat.ignoreUser",
            &[
                ("rid", room_id.to_string()),
                ("userId", user_id.to_string()),
                ("ignore", ignore.to_string()),
            ],
        )
        .await?;
    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    if ignore {
        world.ignored_users.insert(user_id.to_string());
    } else {
        world.ignored_users.remove(user_id);
    }
    Ok(resp)
}
