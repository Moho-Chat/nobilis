//! Room moderation: power-level permission checks, plus kick/ban/unban/
//! mute themselves. Redacting *others'* messages needs no new function
//! here at all - mod.rs's existing delete_message already redacts
//! whatever event_id it's given with no ownership check of its own (the
//! homeserver is what actually enforces who's allowed to redact what);
//! this module's own job is just telling the frontend when to *offer*
//! that (and kick/ban/mute) in the first place.
//!
//! Power levels are handled as raw `serde_json::Value`, matching this
//! backend's existing convention (see crypto.rs's own module doc comment
//! on why) - not worth a dedicated struct for a handful of int lookups.
//!
//! Every permission check here is advisory/UI-gating only: the
//! homeserver is the real authority and will reject an unauthorized
//! action regardless of what's cached locally in Runtime (see
//! roomstate.rs) - a stale or never-populated power_levels cache just
//! means the UI might offer (or hide) an action it shouldn't, not a
//! security boundary.

use super::http;
use crate::state::AppState;
use anyhow::{Context, Result};
use serde_json::Value;

fn threshold(pl: &Value, key: &str, default: i64) -> i64 {
    pl[key].as_i64().unwrap_or(default)
}

pub fn user_power_level(pl: &Value, user_id: &str) -> i64 {
    pl["users"][user_id].as_i64().unwrap_or_else(|| threshold(pl, "users_default", 0))
}

fn message_send_threshold(pl: &Value) -> i64 {
    pl["events"]["m.room.message"].as_i64().unwrap_or_else(|| threshold(pl, "events_default", 0))
}

fn power_levels_change_threshold(pl: &Value) -> i64 {
    pl["events"]["m.room.power_levels"].as_i64().unwrap_or_else(|| threshold(pl, "state_default", 50))
}

/// `{canRedactOthers, canKick, canBan, canMute}` for `own_user_id` in a
/// room, given its current cached power levels (`{}` - every default -
/// for a room where no one's ever customized permissions, or where
/// nothing's been cached yet; both parse through the same defaults above
/// without error). Room-level only, not per-target: the actual spec rules
/// for kick/ban/mute also require the acting user's power to exceed the
/// *target's*, which this can't check without knowing who - the mutation
/// itself still enforces that server-side regardless, this is only ever
/// used to decide whether to show the option at all.
pub fn permissions_json(pl: &Value, own_user_id: &str) -> Value {
    let own_level = user_power_level(pl, own_user_id);
    serde_json::json!({
        "canRedactOthers": own_level >= threshold(pl, "redact", 50),
        "canKick": own_level >= threshold(pl, "kick", 50),
        "canBan": own_level >= threshold(pl, "ban", 50),
        "canMute": own_level >= power_levels_change_threshold(pl),
    })
}

/// This account's own cached permissions in a buffer's room - `{}` (every
/// default) if nothing's been cached yet, same reasoning as
/// permissions_json's own doc comment.
pub fn permissions_for_buffer(state: &AppState, account_id: &str, buffer_id: &str) -> Result<Value> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let account = state.accounts.get_matrix(account_id).context("no such Matrix account")?;
    let pl = state.runtime.get_matrix_power_levels(account_id, &room_id).unwrap_or_else(|| serde_json::json!({}));
    Ok(permissions_json(&pl, &account.user_id))
}

async fn homeserver_token_room(state: &AppState, account_id: &str, buffer_id: &str) -> Result<(String, String, String)> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let account = state.accounts.get_matrix(account_id).context("no such Matrix account")?;
    Ok((account.homeserver_url, account.access_token, room_id))
}

pub async fn kick_member(state: &AppState, account_id: &str, buffer_id: &str, target_user_id: &str, reason: Option<&str>) -> Result<()> {
    let (homeserver_url, access_token, room_id) = homeserver_token_room(state, account_id, buffer_id).await?;
    let base = homeserver_url.trim_end_matches('/');
    let url = format!("{base}/_matrix/client/v3/rooms/{}/kick", url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>());
    let mut body = serde_json::json!({ "user_id": target_user_id });
    if let Some(reason) = reason {
        body["reason"] = Value::String(reason.to_string());
    }
    http::post_json(&url, Some(&access_token), body).await.context("kicking member")?;
    Ok(())
}

pub async fn ban_member(state: &AppState, account_id: &str, buffer_id: &str, target_user_id: &str, reason: Option<&str>) -> Result<()> {
    let (homeserver_url, access_token, room_id) = homeserver_token_room(state, account_id, buffer_id).await?;
    let base = homeserver_url.trim_end_matches('/');
    let url = format!("{base}/_matrix/client/v3/rooms/{}/ban", url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>());
    let mut body = serde_json::json!({ "user_id": target_user_id });
    if let Some(reason) = reason {
        body["reason"] = Value::String(reason.to_string());
    }
    http::post_json(&url, Some(&access_token), body).await.context("banning member")?;
    Ok(())
}

pub async fn unban_member(state: &AppState, account_id: &str, buffer_id: &str, target_user_id: &str) -> Result<()> {
    let (homeserver_url, access_token, room_id) = homeserver_token_room(state, account_id, buffer_id).await?;
    let base = homeserver_url.trim_end_matches('/');
    let url = format!("{base}/_matrix/client/v3/rooms/{}/unban", url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>());
    http::post_json(&url, Some(&access_token), serde_json::json!({ "user_id": target_user_id })).await.context("unbanning member")?;
    Ok(())
}

/// Lowers a member's power level to one below whatever's needed to send
/// `m.room.message` in this room, without removing them from it - "mute"
/// as a power-level floor rather than a membership change. Read-modify-
/// write of the full `m.room.power_levels` state event, since Matrix
/// state events are always a full replace, never a partial patch - a
/// naive "just PUT {users: {target: level}}" would silently wipe every
/// other customization the room's power levels already had.
pub async fn mute_member(state: &AppState, account_id: &str, buffer_id: &str, target_user_id: &str) -> Result<()> {
    let (homeserver_url, access_token, room_id) = homeserver_token_room(state, account_id, buffer_id).await?;
    let base = homeserver_url.trim_end_matches('/');
    let encoded_room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let state_url = format!("{base}/_matrix/client/v3/rooms/{encoded_room}/state/m.room.power_levels/");

    let mut pl = http::get_json(&state_url, &access_token).await.context("fetching current power levels")?;
    let muted_level = message_send_threshold(&pl) - 1;
    pl["users"][target_user_id] = serde_json::json!(muted_level);

    http::put_json(&state_url, &access_token, pl).await.context("updating power levels")?;
    Ok(())
}
