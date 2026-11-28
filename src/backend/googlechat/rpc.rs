//! Google Chat's own daemon methods.
//!
//! - `addGoogleChatAccount`: captures the browser session cookies + user agent, validates them with
//!   `get_self_user_status`, and saves/spawns the account.
//! - `googlechat`: protocol-specific actions (`createDm`, `renameSpace`, `setSpaceDescription`, `inviteMember`,
//!   `kickMember`, `listInvites`, `acceptInvite`, `declineInvite`).

use super::api::Api;
use super::people;
use super::session::{self, Session};
use crate::accounts::GoogleChatAccountConfig;
use crate::state::AppState;
use serde_json::{json, Value};

fn p_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

pub async fn dispatch(state: &AppState, method: &str, params: &Value) -> Option<(Option<Value>, Option<String>)> {
    match method {
        "addGoogleChatAccount" => Some(match add_account(state, params).await {
            Ok(id) => (Some(json!({ "ok": true, "accountId": id })), None),
            Err(e) => (None, Some(e)),
        }),
        "googlechat" => Some(match dispatch_action(state, params).await {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        }),
        _ => None,
    }
}

async fn dispatch_action(state: &AppState, params: &Value) -> Result<Value, String> {
    let action = p_str(params, "action").ok_or("googlechat requires \"action\"")?;
    let buffer = p_str(params, "bufferId").and_then(|id| state.runtime.get_buffer(id));
    let account_id = p_str(params, "accountId")
        .map(str::to_string)
        .or_else(|| buffer.as_ref().map(|b| b.account_id.clone()))
        .ok_or("googlechat requires \"accountId\" or \"bufferId\"")?;
    let handle = state
        .runtime
        .googlechat_handle(&account_id)
        .ok_or("that Google Chat account is not connected")?;

    match action {
        "createDm" => {
            let target = p_str(params, "target").ok_or("createDm requires \"target\" (email or user ID)")?;
            let buf = people::create_dm(state, &handle, target).await?;
            Ok(json!({ "ok": true, "bufferId": buf.id }))
        }
        "createSpace" => {
            let name = p_str(params, "name").ok_or("createSpace requires \"name\"")?;
            let desc = p_str(params, "description");
            let buf = people::create_space(state, &handle, name, desc).await?;
            Ok(json!({ "ok": true, "bufferId": buf.id }))
        }
        "renameSpace" => {
            let buf = buffer.ok_or("renameSpace requires \"bufferId\"")?;
            let name = p_str(params, "name").ok_or("renameSpace requires \"name\"")?;
            people::rename_space(state, &handle, &buf, name).await?;
            Ok(json!({ "ok": true }))
        }
        "setSpaceDescription" => {
            let buf = buffer.ok_or("setSpaceDescription requires \"bufferId\"")?;
            let desc = params.get("description").and_then(Value::as_str).unwrap_or("");
            people::set_space_description(state, &handle, &buf, desc).await?;
            Ok(json!({ "ok": true }))
        }
        "setSpaceDetails" => {
            let buf = buffer.ok_or("setSpaceDetails requires \"bufferId\"")?;
            let desc = params.get("description").and_then(Value::as_str);
            let guidelines = params.get("guidelines").and_then(Value::as_str);
            people::set_space_details(state, &handle, &buf, desc, guidelines).await?;
            Ok(json!({ "ok": true }))
        }
        "listSharedFiles" => {
            let buf = buffer.ok_or("listSharedFiles requires \"bufferId\"")?;
            let files = people::list_shared_files(state, &buf);
            Ok(json!({ "files": files }))
        }
        "markUnread" => {
            let buf = buffer.ok_or("markUnread requires \"bufferId\"")?;
            state.runtime.set_buffer_unread(state, &buf.id, 1, 0);
            Ok(json!({ "ok": true }))
        }
        "joinSpace" => {
            let space_id = p_str(params, "spaceId").ok_or("joinSpace requires \"spaceId\"")?;
            let buf = people::answer_invite(state, &handle, space_id, true).await?;
            Ok(json!({ "ok": true, "bufferId": buf.map(|b| b.id) }))
        }
        "createMeetLink" => {
            let buf = buffer.ok_or("createMeetLink requires \"bufferId\"")?;
            let url = people::create_meet_link(state, &handle, &buf).await?;
            Ok(json!({ "ok": true, "url": url }))
        }
        "listBlockedUsers" => {
            let users = people::list_blocked_users(&handle).await?;
            Ok(json!({ "users": users }))
        }
        "autocompleteCommands" => {
            let buf = buffer.ok_or("autocompleteCommands requires \"bufferId\"")?;
            let query = params.get("query").and_then(Value::as_str).unwrap_or("");
            let commands = people::slash_commands(&handle, &buf, query).await?;
            Ok(json!({ "commands": commands }))
        }
        "searchMessages" => {
            let query = params.get("query").and_then(Value::as_str).unwrap_or("");
            let messages = people::search_messages(&handle, query, buffer.as_ref()).await?;
            Ok(json!({ "messages": messages }))
        }
        "browseSpaces" => {
            let query = params.get("query").and_then(Value::as_str).unwrap_or("");
            let unjoined_only = params.get("unjoinedOnly").and_then(Value::as_bool).unwrap_or(false);
            let spaces = people::browse_spaces(&handle, query, unjoined_only).await?;
            Ok(json!({ "spaces": spaces }))
        }
        "pinConversation" => {
            let buf = buffer.ok_or("pinConversation requires \"bufferId\"")?;
            let pinned = params.get("pinned").and_then(Value::as_bool).unwrap_or(true);
            let result = people::pin_conversation(state, &handle, &buf, pinned).await?;
            Ok(json!({ "ok": true, "pinned": result }))
        }
        "spaceSettings" => {
            let buf = buffer.ok_or("spaceSettings requires \"bufferId\"")?;
            super::space_settings::read(&handle, &buf).await
        }
        "saveSpaceSettings" => {
            let buf = buffer.ok_or("saveSpaceSettings requires \"bufferId\"")?;
            super::space_settings::save(state, &handle, &buf, params).await
        }
        "setSpaceEmoji" => {
            let buf = buffer.ok_or("setSpaceEmoji requires \"bufferId\"")?;
            let emoji = params.get("emoji").and_then(Value::as_str).unwrap_or("");
            people::set_space_emoji(&handle, &buf, emoji).await?;
            Ok(json!({ "ok": true }))
        }
        "setHistoryState" => {
            let buf = buffer.ok_or("setHistoryState requires \"bufferId\"")?;
            let history_on = params.get("historyOn").and_then(Value::as_bool).unwrap_or(true);
            let result = people::set_history_state(state, &handle, &buf, history_on).await?;
            Ok(json!({ "ok": true, "historyOn": result }))
        }
        "setMemberRole" => {
            let buf = buffer.ok_or("setMemberRole requires \"bufferId\"")?;
            let user_id = p_str(params, "userId").ok_or("setMemberRole requires \"userId\"")?;
            let role = p_str(params, "role").ok_or("setMemberRole requires \"role\" (\"member\", \"manager\", or \"owner\")")?;
            people::set_member_role(&handle, &buf, user_id, role).await?;
            Ok(json!({ "ok": true }))
        }
        "createSection" => {
            let name = p_str(params, "name").ok_or("createSection requires \"name\"")?;
            let sort_order = params.get("sortOrder").and_then(Value::as_i64).unwrap_or(2) as i32;
            people::create_section(&handle, name, sort_order).await
        }
        "deleteSection" => {
            let section_id = p_str(params, "sectionId").ok_or("deleteSection requires \"sectionId\"")?;
            people::delete_section(&handle, section_id).await?;
            Ok(json!({ "ok": true }))
        }
        "moveToSection" => {
            let buf = buffer.ok_or("moveToSection requires \"bufferId\"")?;
            let section_id = p_str(params, "sectionId").ok_or("moveToSection requires \"sectionId\"")?;
            people::move_to_section(&handle, &buf, section_id).await?;
            Ok(json!({ "ok": true }))
        }
        "sessionCookies" => Ok(json!({
            "cookies": handle.api.session.cookies(),
            "userAgent": handle.api.session.user_agent(),
        })),
        "inviteMember" => {
            let buf = buffer.ok_or("inviteMember requires \"bufferId\"")?;
            let target = p_str(params, "target").ok_or("inviteMember requires \"target\" (email or user ID)")?;
            people::invite_member(&handle, &buf, target).await?;
            Ok(json!({ "ok": true }))
        }
        "kickMember" => {
            let buf = buffer.ok_or("kickMember requires \"bufferId\"")?;
            let user_id = p_str(params, "userId").ok_or("kickMember requires \"userId\"")?;
            people::kick_member(&handle, &buf, user_id).await?;
            Ok(json!({ "ok": true }))
        }
        "blockUser" => {
            let user_id = p_str(params, "userId").ok_or("blockUser requires \"userId\"")?;
            let blocked = params.get("blocked").and_then(Value::as_bool).unwrap_or(true);
            let reported = params.get("reported").and_then(Value::as_bool).unwrap_or(false);
            people::block_user(&handle, user_id, blocked, reported).await?;
            Ok(json!({ "ok": true }))
        }
        "blockGroup" => {
            let buf = buffer.ok_or("blockGroup requires \"bufferId\"")?;
            let blocked = params.get("blocked").and_then(Value::as_bool).unwrap_or(true);
            let reported = params.get("reported").and_then(Value::as_bool).unwrap_or(false);
            people::block_group(&handle, &buf, blocked, reported).await?;
            Ok(json!({ "ok": true }))
        }
        "setCustomStatus" => {
            let text = params.get("text").and_then(Value::as_str).unwrap_or("");
            let emoji = p_str(params, "emoji");
            let duration_secs = params.get("durationSecs").and_then(Value::as_i64);
            people::set_custom_status(&handle, text, emoji, duration_secs).await?;
            Ok(json!({ "ok": true }))
        }
        "setDnd" => {
            let dnd = params.get("dnd").and_then(Value::as_bool).unwrap_or(true);
            let duration_secs = params.get("durationSecs").and_then(Value::as_i64);
            people::set_dnd(&handle, dnd, duration_secs).await?;
            Ok(json!({ "ok": true }))
        }
        "listInvites" => {
            let invites = people::pending_invites_json(&handle).await;
            Ok(json!({ "invites": invites }))
        }
        "acceptInvite" => {
            let space_id = p_str(params, "spaceId").ok_or("acceptInvite requires \"spaceId\"")?;
            let buf = people::answer_invite(state, &handle, space_id, true).await?;
            Ok(json!({ "ok": true, "bufferId": buf.map(|b| b.id) }))
        }
        "declineInvite" => {
            let space_id = p_str(params, "spaceId").ok_or("declineInvite requires \"spaceId\"")?;
            people::answer_invite(state, &handle, space_id, false).await?;
            Ok(json!({ "ok": true }))
        }
        other => Err(format!("unknown googlechat action {other:?}")),
    }
}

async fn add_account(state: &AppState, params: &Value) -> Result<String, String> {
    let cookies = session::parse_cookies(p_str(params, "cookies").ok_or("addGoogleChatAccount requires \"cookies\"")?);
    let missing = session::missing_cookies(&cookies);
    if !missing.is_empty() {
        return Err(format!("The Google sign-in did not finish: the session is missing {}. Try signing in again", missing.join(", ")));
    }
    let user_agent = p_str(params, "userAgent").unwrap_or(session::DEFAULT_USER_AGENT).to_string();
    let use_tor = params.get("useTor").and_then(Value::as_bool).unwrap_or(false);

    let key = crate::net::route::pending_key("googlechat");
    let router = crate::net::route::router();
    router.mark(&key, use_tor);
    if use_tor || router.tunnel_all() {
        router.ready(|_| {}).await.map_err(|e| format!("Tor or the proxy is not ready: {e:#}"))?;
    }
    let api = Api::new(Session::new(&key, cookies, &user_agent));
    let status = api.self_user_status().await.map_err(|e| e.to_string())?;
    let gaia = status.user_status.and_then(|s| s.user_id).map(|u| u.id().to_string()).filter(|s| !s.is_empty()).ok_or("Google Chat did not say whose session this is")?;
    // Re-signing an account from its menu must be the same Google account, or it would quietly become another one.
    if let Some(expected) = p_str(params, "accountId") {
        if expected != super::account_id(&gaia) {
            return Err("That sign-in was a different Google account from this one. Add it as a new account instead".into());
        }
    }
    let (mut name, mut email) = (String::new(), String::new());
    if let Ok(response) = api.members(std::slice::from_ref(&gaia)).await {
        for member in &response.members {
            if let Some(super::pb::member::Profile::User(user)) = &member.profile {
                name = user.name().to_string();
                email = user.email().to_string();
            }
        }
    }
    let config = GoogleChatAccountConfig {
        user_id: gaia,
        email,
        name,
        cookies: api.session.cookies(),
        user_agent,
        use_tor,
        ..Default::default()
    };
    let saved = state.accounts.add_googlechat(config).map_err(|e| format!("{e:#}"))?;
    let id = saved.account_id();
    state.accounts.set_disconnected(&id, false).ok();
    super::spawn(state.clone(), saved);
    state.events.emit("accountsChanged", json!({}));
    Ok(id)
}
