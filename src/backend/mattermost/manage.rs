//! Joining and leaving, and looking after a channel or a team: making channels, browsing those open to join, a
//! channel's settings (name, header, purpose, privacy, archive), its members and their roles, a team's members and
//! roles, invite links, and the integrations a team has.
//!
//! What may be done is the server's to say; the window asks `powers` first and offers only that, and anything refused
//! anyway comes back in the server's words.

use super::{events, messages, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

fn err(e: super::api::ApiError) -> anyhow::Error {
    anyhow!("{e}")
}

/// What was typed into the join box, read: an invite link's id, `team/channel`, a team, or a channel.
#[derive(Debug, PartialEq)]
pub enum Target {
    Invite(String),
    TeamChannel(String, String),
    Name(String),
}

pub fn read_target(typed: &str) -> Option<Target> {
    let text = typed.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(url) = url::Url::parse(text) {
        if let Some((_, id)) = url.query_pairs().find(|(k, _)| k == "id") {
            return Some(Target::Invite(id.to_string()));
        }
        // A channel's address in the web app: `/<team>/channels/<channel>`.
        let parts: Vec<&str> = url.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
        if let Some(at) = parts.iter().position(|p| *p == "channels") {
            if at >= 1 && at + 1 < parts.len() {
                return Some(Target::TeamChannel(parts[at - 1].to_string(), parts[at + 1].to_string()));
            }
        }
        if parts.len() == 1 {
            return Some(Target::Name(parts[0].to_string()));
        }
        return None;
    }
    if let Some((team, channel)) = text.split_once('/') {
        return Some(Target::TeamChannel(team.trim().to_string(), channel.trim().trim_start_matches('~').to_string()));
    }
    Some(Target::Name(text.trim_start_matches('~').to_string()))
}

/// Joins a team (and so its Town Square), and reads everything again.
async fn join_team(state: &AppState, handle: &Handle, team_id: &str) -> Result<()> {
    let own = handle.own_id();
    handle.api.post(&format!("/api/v4/teams/{team_id}/members"), &json!({ "team_id": team_id, "user_id": own })).await.map_err(err)?;
    events::load(state, handle).await;
    Ok(())
}

/// Joins a channel the account may see, and makes its conversation.
async fn join_channel(state: &AppState, handle: &Handle, channel_id: &str) -> Result<()> {
    let own = handle.own_id();
    handle.api.post(&format!("/api/v4/channels/{channel_id}/members"), &json!({ "user_id": own })).await.map_err(err)?;
    events::fetch_channel(state, handle, channel_id).await;
    Ok(())
}

/// A team by its name in addresses or what it is called, among those the account is in or may join.
async fn find_team(handle: &Handle, name: &str) -> Option<String> {
    let lowered = name.to_lowercase();
    if let Some(id) = handle.world.lock().unwrap_or_else(|e| e.into_inner()).teams.values().find(|t| t.name == lowered || t.display_name.to_lowercase() == lowered).map(|t| t.id.clone()) {
        return Some(id);
    }
    if let Ok(t) = handle.api.get(&format!("/api/v4/teams/name/{lowered}"), &[]).await {
        return t["id"].as_str().map(str::to_string);
    }
    let open = handle.api.get("/api/v4/teams", &[("per_page", "200".into())]).await.ok()?;
    open.as_array().into_iter().flatten().find(|t| t["display_name"].as_str().map(str::to_lowercase).as_deref() == Some(lowered.as_str())).and_then(|t| t["id"].as_str().map(str::to_string))
}

/// Joins what was typed: an invite link to a team, `team/channel`, a team that is open to join, or a channel of one of
/// the account's teams by name.
pub async fn join(state: &AppState, handle: &Handle, typed: &str) -> Result<()> {
    match read_target(typed).ok_or_else(|| anyhow!("type a team, a channel, or an invite link"))? {
        Target::Invite(id) => {
            handle.api.post(&format!("/api/v4/teams/members/invite?invite_id={}", super::send::urlencode(&id)), &json!({})).await.map_err(err)?;
            events::load(state, handle).await;
            Ok(())
        }
        Target::TeamChannel(team, channel) => {
            let team_id = find_team(handle, &team).await.ok_or_else(|| anyhow!("there is no team called {team} that this account can see"))?;
            if !handle.world.lock().unwrap_or_else(|e| e.into_inner()).teams.contains_key(&team_id) {
                join_team(state, handle, &team_id).await?;
            }
            let found = handle.api.get(&format!("/api/v4/teams/{team_id}/channels/name/{}", channel.to_lowercase()), &[]).await.map_err(err)?;
            let id = found["id"].as_str().ok_or_else(|| anyhow!("there is no channel called {channel} in {team}"))?.to_string();
            join_channel(state, handle, &id).await
        }
        Target::Name(name) => {
            let teams: Vec<String> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).team_order.clone();
            for team in &teams {
                let found = handle.api.post(&format!("/api/v4/teams/{team}/channels/search"), &json!({ "term": name })).await.unwrap_or(Value::Null);
                let lowered = name.to_lowercase();
                if let Some(id) = found.as_array().into_iter().flatten().find(|c| c["name"].as_str() == Some(lowered.as_str()) || c["display_name"].as_str().map(str::to_lowercase).as_deref() == Some(lowered.as_str())).and_then(|c| c["id"].as_str()) {
                    return join_channel(state, handle, id).await;
                }
            }
            match find_team(handle, &name).await {
                Some(team_id) => join_team(state, handle, &team_id).await,
                None => bail!("no team or channel called {name} was found"),
            }
        }
    }
}

/// Leaves a channel.
pub async fn leave_channel(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<()> {
    let channel = buffer.remote_id.clone().ok_or_else(|| anyhow!("this is not a Mattermost conversation"))?;
    let kind = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(&channel).map(|c| c.kind.clone()).unwrap_or_default();
    if kind == "D" {
        // A direct message cannot be left: it is closed (hidden) as Mattermost's app does.
        let own = handle.own_id();
        handle.api.put(&format!("/api/v4/users/{own}/preferences"), &json!([{ "user_id": own, "category": "direct_channel_show", "name": channel, "value": "false" }])).await.map_err(err)?;
    } else {
        let own = handle.own_id();
        handle.api.delete(&format!("/api/v4/channels/{channel}/members/{own}")).await.map_err(err)?;
    }
    handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.remove(&channel);
    state.runtime.remove_buffer(state, &buffer.id);
    Ok(())
}

/// Leaves a team, and everything in it.
pub async fn leave_team(state: &AppState, handle: &Handle, team: &str) -> Result<()> {
    let own = handle.own_id();
    handle.api.delete(&format!("/api/v4/teams/{team}/members/{own}")).await.map_err(err)?;
    events::load(state, handle).await;
    Ok(())
}

/// The teams of the server the account could join.
pub async fn open_teams(handle: &Handle) -> Result<Value> {
    let list = handle.api.get("/api/v4/teams", &[("per_page", "200".into())]).await.map_err(err)?;
    let mine: std::collections::HashSet<String> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).teams.keys().cloned().collect();
    Ok(json!(list
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["allow_open_invite"].as_bool() == Some(true) || t["type"].as_str() == Some("O"))
        .filter(|t| !t["id"].as_str().is_some_and(|id| mine.contains(id)))
        .map(|t| json!({ "id": t["id"], "name": t["name"], "displayName": t["display_name"], "description": t["description"] }))
        .collect::<Vec<_>>()))
}

/// The open channels of a team, or those matching some words.
pub async fn browse(handle: &Handle, team: &str, term: &str) -> Result<Value> {
    let list = if term.trim().is_empty() {
        handle.api.get(&format!("/api/v4/teams/{team}/channels"), &[("per_page", "200".into())]).await
    } else {
        handle.api.post(&format!("/api/v4/teams/{team}/channels/search"), &json!({ "term": term.trim() })).await
    }
    .map_err(err)?;
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    Ok(json!(list
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["type"].as_str() == Some("O"))
        .map(|c| json!({ "id": c["id"], "name": c["name"], "displayName": c["display_name"], "purpose": c["purpose"], "joined": c["id"].as_str().is_some_and(|id| world.channels.contains_key(id)) }))
        .collect::<Vec<_>>()))
}

/// Makes a channel in a team: open or private.
pub async fn create_channel(state: &AppState, handle: &Handle, team: &str, display_name: &str, private: bool, purpose: &str) -> Result<String> {
    let display = display_name.trim();
    if display.is_empty() {
        bail!("give the channel a name");
    }
    let name = url_name(display);
    let body = json!({ "team_id": team, "name": name, "display_name": display, "type": if private { "P" } else { "O" }, "purpose": purpose });
    let made = handle.api.post("/api/v4/channels", &body).await.map_err(err)?;
    let id = made["id"].as_str().ok_or_else(|| anyhow!("the server did not say what it made"))?.to_string();
    events::fetch_channel(state, handle, &id).await;
    messages::buffer_of(state, &handle.account_id, &id).map(|b| b.id).ok_or_else(|| anyhow!("the channel did not come up"))
}

/// A channel's name in addresses, from what it is called: lowercase letters, digits and dashes, as Mattermost's app
/// makes it; a name with none of those gets a made-up one, as the app does.
pub fn url_name(display: &str) -> String {
    let mut out = String::new();
    for c in display.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').chars().take(64).collect::<String>();
    if out.len() < 2 {
        format!("c{}", chrono::Utc::now().timestamp_millis())
    } else {
        out
    }
}

/// What the account may do in a channel and its team, for the window to offer only that.
pub fn powers(handle: &Handle, buffer: &Buffer) -> Value {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let Some(channel) = buffer.remote_id.as_deref().and_then(|c| world.channels.get(c)) else { return json!({}) };
    let open = channel.kind == "O";
    let (props, members, delete) = if open { ("manage_public_channel_properties", "manage_public_channel_members", "delete_public_channel") } else { ("manage_private_channel_properties", "manage_private_channel_members", "delete_private_channel") };
    let team = &channel.team_id;
    json!({
        "kind": channel.kind,
        "teamId": team,
        "channelProperties": !channel.is_personal() && world.may(channel, props),
        "channelMembers": !channel.is_personal() && world.may(channel, members),
        "channelRoles": !channel.is_personal() && world.may(channel, "manage_channel_roles"),
        "archive": !channel.is_personal() && world.may(channel, delete),
        "convertToPrivate": open && world.may(channel, "convert_public_channel_to_private"),
        "convertToPublic": channel.kind == "P" && world.may(channel, "convert_private_channel_to_public"),
        "createPublic": !team.is_empty() && world.may_in_team(team, "create_public_channel"),
        "createPrivate": !team.is_empty() && world.may_in_team(team, "create_private_channel"),
        "teamMembers": !team.is_empty() && world.may_in_team(team, "manage_team"),
        "teamRoles": !team.is_empty() && world.may_in_team(team, "manage_team_roles"),
        "teamInvite": !team.is_empty() && world.may_in_team(team, "invite_user"),
        "webhooks": !team.is_empty() && (world.may_in_team(team, "manage_incoming_webhooks") || world.may_in_team(team, "manage_outgoing_webhooks")),
        "moderation": !channel.is_personal() && world.may_in_team(team, "manage_channel_roles"),
        "archived": channel.archived,
        "channelAdmin": world.members.get(&channel.id).is_some_and(|m| m.admin),
    })
}

/// The channel a buffer is, with the team it is in.
fn channel_team(handle: &Handle, buffer: &Buffer) -> Result<(String, String)> {
    let channel = buffer.remote_id.clone().ok_or_else(|| anyhow!("this is not a Mattermost conversation"))?;
    let team = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(&channel).map(|c| c.team_id.clone()).unwrap_or_default();
    Ok((channel, team))
}

/// Runs one of the window's actions on a channel or its team. `action` names it; `params` carries what it needs.
pub async fn run(state: &AppState, handle: &Handle, buffer: &Buffer, action: &str, params: &Value) -> Result<Value> {
    let (channel, team) = channel_team(handle, buffer)?;
    let text = |k: &str| params[k].as_str().unwrap_or("").to_string();
    let user = text("userId");
    match action {
        "settings" => {
            let c = handle.api.get(&format!("/api/v4/channels/{channel}"), &[]).await.map_err(err)?;
            let stats = handle.api.get(&format!("/api/v4/channels/{channel}/stats"), &[]).await.unwrap_or(Value::Null);
            Ok(json!({ "displayName": c["display_name"], "name": c["name"], "header": c["header"], "purpose": c["purpose"], "kind": c["type"], "archived": c["delete_at"].as_i64().unwrap_or(0) > 0, "members": stats["member_count"], "pinned": stats["pinnedpost_count"], "powers": powers(handle, buffer) }))
        }
        "patch" => {
            let mut patch = serde_json::Map::new();
            for (from, to) in [("displayName", "display_name"), ("header", "header"), ("purpose", "purpose")] {
                if let Some(v) = params[from].as_str() {
                    patch.insert(to.to_string(), json!(v));
                }
            }
            handle.api.put(&format!("/api/v4/channels/{channel}/patch"), &Value::Object(patch)).await.map_err(err)?;
            Ok(json!({ "ok": true }))
        }
        "privacy" => {
            let private = params["private"].as_bool().unwrap_or(true);
            handle.api.put(&format!("/api/v4/channels/{channel}/privacy"), &json!({ "privacy": if private { "P" } else { "O" } })).await.map_err(err)?;
            Ok(json!({ "ok": true }))
        }
        "archive" => {
            handle.api.delete(&format!("/api/v4/channels/{channel}")).await.map_err(err)?;
            Ok(json!({ "ok": true }))
        }
        "restore" => {
            handle.api.post(&format!("/api/v4/channels/{channel}/restore"), &json!({})).await.map_err(err)?;
            events::fetch_channel(state, handle, &channel).await;
            Ok(json!({ "ok": true }))
        }
        "members" => {
            let list = handle.api.get(&format!("/api/v4/channels/{channel}/members"), &[("per_page", "200".into())]).await.map_err(err)?;
            let ids: Vec<Value> = list.as_array().into_iter().flatten().map(|m| json!({ "user_id": m["user_id"] })).collect();
            messages::learn_authors(handle, &ids.iter().collect::<Vec<_>>()).await;
            let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            Ok(json!(list.as_array().into_iter().flatten().map(|m| {
                let id = m["user_id"].as_str().unwrap_or("");
                json!({ "userId": id, "name": world.name_of(id, &handle.server.teammate_name), "username": world.username_of(id), "admin": m["scheme_admin"].as_bool().unwrap_or(false) })
            }).collect::<Vec<_>>()))
        }
        "addMember" => {
            let id = resolve_user(handle, &user).await?;
            handle.api.post(&format!("/api/v4/channels/{channel}/members"), &json!({ "user_id": id })).await.map_err(err)?;
            Ok(json!({ "ok": true }))
        }
        "removeMember" => {
            handle.api.delete(&format!("/api/v4/channels/{channel}/members/{user}")).await.map_err(err)?;
            Ok(json!({ "ok": true }))
        }
        "channelAdmin" => {
            let admin = params["admin"].as_bool().unwrap_or(true);
            handle.api.put(&format!("/api/v4/channels/{channel}/members/{user}/schemeRoles"), &json!({ "scheme_admin": admin, "scheme_user": true })).await.map_err(err)?;
            Ok(json!({ "ok": true }))
        }
        "teamMembers" => {
            let list = handle.api.get(&format!("/api/v4/teams/{team}/members"), &[("per_page", "200".into())]).await.map_err(err)?;
            let ids: Vec<Value> = list.as_array().into_iter().flatten().map(|m| json!({ "user_id": m["user_id"] })).collect();
            messages::learn_authors(handle, &ids.iter().collect::<Vec<_>>()).await;
            let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            Ok(json!(list.as_array().into_iter().flatten().filter(|m| m["delete_at"].as_i64().unwrap_or(0) == 0).map(|m| {
                let id = m["user_id"].as_str().unwrap_or("");
                json!({ "userId": id, "name": world.name_of(id, &handle.server.teammate_name), "username": world.username_of(id), "admin": m["scheme_admin"].as_bool().unwrap_or(false) })
            }).collect::<Vec<_>>()))
        }
        "teamAdmin" => {
            let admin = params["admin"].as_bool().unwrap_or(true);
            handle.api.put(&format!("/api/v4/teams/{team}/members/{user}/schemeRoles"), &json!({ "scheme_admin": admin, "scheme_user": true })).await.map_err(err)?;
            Ok(json!({ "ok": true }))
        }
        "addTeamMember" => {
            let id = resolve_user(handle, &user).await?;
            handle.api.post(&format!("/api/v4/teams/{team}/members"), &json!({ "team_id": team, "user_id": id })).await.map_err(err)?;
            Ok(json!({ "ok": true }))
        }
        "removeTeamMember" => {
            handle.api.delete(&format!("/api/v4/teams/{team}/members/{user}")).await.map_err(err)?;
            Ok(json!({ "ok": true }))
        }
        "invite" => {
            let t = handle.api.get(&format!("/api/v4/teams/{team}"), &[]).await.map_err(err)?;
            let id = t["invite_id"].as_str().unwrap_or("");
            if id.is_empty() {
                bail!("This team has no invite link the account may see");
            }
            Ok(json!({ "link": format!("{}/signup_user_complete/?id={id}", handle.api.base()), "inviteId": id }))
        }
        "newInvite" => {
            let t = handle.api.post(&format!("/api/v4/teams/{team}/regenerate_invite_id"), &json!({})).await.map_err(err)?;
            let id = t["invite_id"].as_str().unwrap_or("");
            Ok(json!({ "link": format!("{}/signup_user_complete/?id={id}", handle.api.base()), "inviteId": id }))
        }
        "inviteEmail" => {
            let emails: Vec<String> = text("emails").split([',', ' ', '\n']).map(str::trim).filter(|e| e.contains('@')).map(str::to_string).collect();
            if emails.is_empty() {
                bail!("give one or more email addresses");
            }
            handle.api.post(&format!("/api/v4/teams/{team}/invite/email"), &json!(emails)).await.map_err(err)?;
            Ok(json!({ "ok": true, "sent": emails.len() }))
        }
        "webhooks" => {
            let query = [("team_id", team.clone()), ("per_page", "100".to_string())];
            let (incoming, outgoing) = tokio::join!(handle.api.get("/api/v4/hooks/incoming", &query), handle.api.get("/api/v4/hooks/outgoing", &query));
            let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            let describe = |h: &Value, kind: &str| {
                let channel = h["channel_id"].as_str().and_then(|c| world.channels.get(c)).map(|c| world.title_of(c, &handle.server.teammate_name));
                json!({ "id": h["id"], "kind": kind, "name": h["display_name"], "description": h["description"], "channel": channel, "by": world.name_of(h["creator_id"].as_str().unwrap_or(""), &handle.server.teammate_name), "triggers": h["trigger_words"] })
            };
            let mut out: Vec<Value> = incoming.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default().iter().map(|h| describe(h, "incoming")).collect();
            out.extend(outgoing.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default().iter().map(|h| describe(h, "outgoing")));
            Ok(json!(out))
        }
        "moderation" => {
            let list = handle.api.get(&format!("/api/v4/channels/{channel}/moderations"), &[]).await.map_err(err)?;
            Ok(list)
        }
        "setModeration" => {
            // One permission (`create_post`, `create_reactions`, `manage_members`, `use_channel_mentions`) for members or
            // guests, on or off.
            let body = json!([{ "name": text("permission"), "roles": { text("role"): params["on"].as_bool().unwrap_or(true) } }]);
            let list = handle.api.put(&format!("/api/v4/channels/{channel}/moderations/patch"), &body).await.map_err(err)?;
            Ok(list)
        }
        other => bail!("there is no such action as {other}"),
    }
}

/// A person by id or username.
async fn resolve_user(handle: &Handle, who: &str) -> Result<String> {
    if super::is_id(who) {
        return Ok(who.to_string());
    }
    let found = handle.api.get(&format!("/api/v4/users/username/{}", who.trim_start_matches('@')), &[]).await.map_err(err)?;
    found["id"].as_str().map(str::to_string).ok_or_else(|| anyhow!("nobody on this server is called {who}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_was_typed_to_join_is_read() {
        assert_eq!(read_target("https://chat.example.org/signup_user_complete/?id=abc123"), Some(Target::Invite("abc123".into())));
        assert_eq!(read_target("https://chat.example.org/alpha/channels/off-topic"), Some(Target::TeamChannel("alpha".into(), "off-topic".into())));
        assert_eq!(read_target("alpha/~off-topic"), Some(Target::TeamChannel("alpha".into(), "off-topic".into())));
        assert_eq!(read_target("~off-topic"), Some(Target::Name("off-topic".into())));
        assert_eq!(read_target("beta"), Some(Target::Name("beta".into())));
        assert_eq!(read_target("  "), None);
    }

    #[test]
    fn a_channels_name_in_addresses_is_made_from_what_it_is_called() {
        assert_eq!(url_name("Off Topic!"), "off-topic");
        assert_eq!(url_name("  Ünïcode -- Room  "), "n-code-room");
        assert!(url_name("日本").starts_with('c'));
    }
}
