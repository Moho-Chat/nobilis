//! A conversation's settings, for the window's settings dialog: what a space is called, its emoji, description and
//! guidelines, and whether history is on - read fresh from Google, and saved one change at a time.
//!
//! These are what a consumer Google account's spaces have (the web app's "Space details" and "Turn off history", seen
//! live 2026-11-27). Access level (restricted / discoverable) and the permission settings (who may post, add people,
//! @all...) are Google Workspace's and are not offered by a consumer account, so nothing here guesses at them.

use super::{people, pb, Group, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use serde_json::{json, Value};

fn group_of(buffer: &Buffer) -> Result<Group, String> {
    buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or_else(|| "not a Google Chat conversation".to_string())
}

/// The conversation's settings as they are now.
pub async fn read(handle: &Handle, buffer: &Buffer) -> Result<Value, String> {
    let group = group_of(buffer)?;
    let response = handle.api.get_group(&group).await.map_err(|e| e.to_string())?;
    let g = response.group.unwrap_or_default();
    let details = g.group_details.clone().unwrap_or_default();
    let history_off = super::retention::is_ephemeral(g.retention_settings.as_ref());
    let own_id = handle.world().own_id.clone();
    let own_role = response
        .memberships
        .iter()
        .find(|m| m.id.as_ref().and_then(|i| i.member_id.as_ref()).and_then(|m| m.id.as_ref()).is_some_and(|id| matches!(id, pb::member_id::Id::UserId(u) if u.id() == own_id)))
        .map(|m| m.membership_role());
    let manager = matches!(own_role, Some(pb::MembershipRole::RoleOwner));
    let (emoji, name) = {
        let mut world = handle.world();
        let conversation = world.conversations.entry(group.clone()).or_default();
        conversation.history_off = history_off;
        (conversation.emoji.clone().unwrap_or_default(), g.name.clone().or_else(|| conversation.name.clone()).unwrap_or_default())
    };
    Ok(json!({
        "isSpace": !group.is_dm(),
        "name": name,
        "emoji": emoji,
        "description": details.description.unwrap_or_default(),
        "guidelines": details.guidelines.unwrap_or_default(),
        "historyOn": !history_off,
        // Whether this account manages the space. Google decides what it accepts; this only says what to expect.
        "manager": manager,
    }))
}

/// Applies what changed. Answers the conversation's buffer id, which a rename changes.
pub async fn save(state: &AppState, handle: &Handle, buffer: &Buffer, params: &Value) -> Result<Value, String> {
    let group = group_of(buffer)?;
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_string);
    let mut done: Vec<&str> = Vec::new();
    if let Some(on) = params.get("historyOn").and_then(Value::as_bool) {
        people::set_history_state(state, handle, buffer, on).await.map_err(|e| format!("history: {e}"))?;
        done.push("history");
    }
    if group.is_dm() {
        return Ok(json!({ "ok": true, "bufferId": buffer.id, "changed": done }));
    }
    if let Some(emoji) = text("emoji") {
        people::set_space_emoji(handle, buffer, &emoji).await.map_err(|e| format!("emoji: {e}"))?;
        let mut world = handle.world();
        if let Some(c) = world.conversations.get_mut(&group) {
            c.emoji = Some(emoji.trim().to_string()).filter(|e| !e.is_empty());
        }
        done.push("emoji");
    }
    let description = text("description");
    let guidelines = text("guidelines");
    if description.is_some() || guidelines.is_some() {
        people::set_space_details(state, handle, buffer, description.as_deref(), guidelines.as_deref()).await.map_err(|e| format!("details: {e}"))?;
        done.push("details");
    }
    let mut buffer_id = buffer.id.clone();
    if let Some(name) = text("name").map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) {
        people::rename_space(state, handle, buffer, &name).await.map_err(|e| format!("name: {e}"))?;
        if let Some(b) = super::world::buffer_of(state, &handle.account_id, &group) {
            buffer_id = b.id;
        }
        done.push("name");
    }
    Ok(json!({ "ok": true, "bufferId": buffer_id, "changed": done }))
}
