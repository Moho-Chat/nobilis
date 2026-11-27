//! Rocket.Chat presence, user profiles, own profile/avatar editing,
//! per-room notification preferences, and user preferences.

use super::Handle;
use anyhow::Result;
use serde_json::{json, Value};

/// Fetch a user's presence (`GET /api/v1/users.getPresence`).
pub async fn get_presence(handle: &Handle, user_id_or_username: &str) -> Result<Value> {
    Ok(handle
        .api
        .get("/api/v1/users.getPresence", &[("userId", user_id_or_username.to_string())])
        .await?)
}

/// Set own presence status (`online`, `away`, `busy`, `offline`) and optional custom status message.
pub async fn set_status(handle: &Handle, status: &str, message: Option<&str>) -> Result<Value> {
    let rc_status = match status {
        "idle" => "away",
        "dnd" => "busy",
        "invisible" => "offline",
        other => other,
    };
    let mut body = json!({ "status": rc_status });
    if let Some(msg) = message {
        body["message"] = json!(msg);
    }
    Ok(handle.api.post("/api/v1/users.setStatus", &body).await?)
}

/// Fetch user profile details (`GET /api/v1/users.info`) for moho's profile popout.
pub async fn user_info(handle: &Handle, user_id: Option<&str>, username: Option<&str>) -> Result<Value> {
    let query = if let Some(uid) = user_id.filter(|s| !s.is_empty()) {
        vec![("userId", uid.to_string())]
    } else if let Some(u) = username.filter(|s| !s.is_empty()) {
        vec![("username", u.to_string())]
    } else {
        anyhow::bail!("user_info requires userId or username");
    };
    let resp = handle.api.get("/api/v1/users.info", &query).await?;
    let u = &resp["user"];
    let uname = u["username"].as_str().unwrap_or("");
    let avatar_url = if !uname.is_empty() {
        Some(format!("{}/avatar/{uname}", handle.api.base()))
    } else {
        None
    };
    Ok(json!({
        "id": u["_id"],
        "username": uname,
        "name": u["name"],
        "nickname": u["nickname"],
        "bio": u["bio"],
        "status": u["status"],
        "statusText": u["statusText"],
        "roles": u["roles"],
        "utcOffset": u["utcOffset"],
        "avatarUrl": avatar_url,
        "federated": super::world::World::is_federated_username(uname),
    }))
}

/// Edit own basic profile info (`POST /api/v1/users.updateOwnBasicInfo`).
pub async fn update_own_profile(
    handle: &Handle,
    name: Option<&str>,
    username: Option<&str>,
    email: Option<&str>,
    status_text: Option<&str>,
    bio: Option<&str>,
    nickname: Option<&str>,
) -> Result<Value> {
    let mut data = json!({});
    if let Some(v) = name { data["name"] = json!(v); }
    if let Some(v) = username { data["username"] = json!(v); }
    if let Some(v) = email { data["email"] = json!(v); }
    let mut custom = json!({});
    if let Some(v) = bio { custom["bio"] = json!(v); }
    if let Some(v) = nickname { custom["nickname"] = json!(v); }
    if let Some(v) = status_text {
        let _ = set_status(handle, "online", Some(v)).await;
    }
    Ok(handle
        .api
        .post("/api/v1/users.updateOwnBasicInfo", &json!({ "data": data, "customFields": custom }))
        .await?)
}

/// Set own avatar by URL (`POST /api/v1/users.setAvatar`).
pub async fn set_avatar_url(handle: &Handle, avatar_url: &str) -> Result<Value> {
    Ok(handle
        .api
        .post("/api/v1/users.setAvatar", &json!({ "avatarUrl": avatar_url }))
        .await?)
}

/// Save per-room notification preferences (`POST /api/v1/rooms.saveNotification`).
pub async fn save_room_notification(
    handle: &Handle,
    room_id: &str,
    notifications: Value,
) -> Result<Value> {
    Ok(handle
        .api
        .post(
            "/api/v1/rooms.saveNotification",
            &json!({ "roomId": room_id, "notifications": notifications }),
        )
        .await?)
}

/// Get user preferences (`GET /api/v1/users.getPreferences`).
pub async fn get_preferences(handle: &Handle) -> Result<Value> {
    Ok(handle.api.get("/api/v1/users.getPreferences", &[]).await?)
}

/// Set user preferences (`POST /api/v1/users.setPreferences`).
pub async fn set_preferences(handle: &Handle, data: Value) -> Result<Value> {
    Ok(handle
        .api
        .post("/api/v1/users.setPreferences", &json!({ "data": data }))
        .await?)
}
