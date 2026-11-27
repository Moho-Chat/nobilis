//! The first load, which makes the rail and the conversations, and what the websocket says after it: posts, edits,
//! deletions, reactions, channels and teams changing, people's status, somebody typing, a channel read elsewhere.
//!
//! A team is a group on the rail; its channels are filed under the team's own sidebar categories (Favorites, Channels,
//! the person's own), in the person's order. Direct and group messages belong to no team and are under Direct Messages.
//! A team channel's conversation is named `Team/Channel`, so two teams' Town Squares are two conversations.

use super::world::{self, Channel};
use super::{calls, media, messages, people, Handle};
use crate::model::BufferGroup;
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::HashSet;

/// How long somebody's "typing" lasts without another: Mattermost sends one every few seconds while they type.
const TYPING_FOR_MS: u64 = 6000;

/// What an event means for the connection.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Carry,
    /// The account's sessions were ended: this one with them.
    SignedOut,
}

/// The rail entry for a team.
pub fn rail_id(account_id: &str, team_id: &str) -> String {
    format!("{account_id}|team:{team_id}")
}

/// A name with its slashes turned into ones that do not divide a conversation's name.
fn unslashed(name: &str) -> String {
    name.replace('/', "∕")
}

/// A team on the rail, with its name and icon.
fn register_team(state: &AppState, handle: &Handle, team: &world::Team, position: i64) {
    state.runtime.upsert_buffer_group(
        state,
        BufferGroup {
            id: rail_id(&handle.account_id, &team.id),
            account_id: handle.account_id.clone(),
            service: "mattermost".to_string(),
            kind: "guild".to_string(),
            name: team.display_name.clone(),
            icon_url: media::team_icon(&handle.account_id, &team.id, team.icon_update),
            position,
            pending: false,
        },
    );
}

/// What a name is in the list: the title, and where it is filed. A name another channel already has is told apart by
/// the end of this one's id.
fn unique_name(state: &AppState, account_id: &str, wanted: &str, channel_id: &str) -> String {
    let taken = state.runtime.list_buffers().into_iter().any(|b| b.account_id == account_id && b.name == wanted && b.remote_id.as_deref() != Some(channel_id));
    if taken {
        format!("{wanted} ({})", &channel_id[channel_id.len().saturating_sub(4)..])
    } else {
        wanted.to_string()
    }
}

/// A channel's conversation: made if it is not there, renamed if it was called something else, filed under its team and
/// its category, and told whether the account may write in it.
pub fn register_channel(state: &AppState, handle: &Handle, channel: &Channel) {
    let account_id = &handle.account_id;
    let style = handle.server.teammate_name.clone();
    let collapsed = messages::collapsed(handle);
    let (wanted, read_only, filed, avatar, unread, muted) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let title = world.title_of(channel, &style);
        let wanted = match world.teams.get(&channel.team_id) {
            Some(team) if !channel.is_personal() => format!("{}/{}", unslashed(&team.display_name), unslashed(&title)),
            _ => unslashed(&title),
        };
        let avatar = channel.other(&world.own_id).map(|o| media::avatar(account_id, &o, world.users.get(&o).map(|u| u.picture).unwrap_or(0)));
        let muted = world.members.get(&channel.id).is_some_and(|m| m.mark_unread == "mention");
        let level = world.members.get(&channel.id).map(|m| people::level_of(&m.notify)).unwrap_or_default();
        (wanted, world.read_only(channel), world.filed(channel), avatar, world.unread(channel, collapsed), (muted, level))
    };
    let (muted, level) = muted;
    let kind = if channel.kind == "D" { "dm" } else { "channel" };
    let existing = messages::buffer_of(state, account_id, &channel.id);
    let name = unique_name(state, account_id, &wanted, &channel.id);
    let buffer = match existing {
        // Through ensure_buffer even when it is there: that is what confirms a conversation put back from the last run.
        Some(found) if found.name == name => state.runtime.ensure_buffer(state, account_id, &name, kind),
        Some(found) => {
            state.runtime.rename_buffer(state, account_id, &found.name, &name, kind);
            state.runtime.get_buffer(&crate::model::buffer_id(account_id, &name)).unwrap_or(found)
        }
        None => state.runtime.ensure_buffer(state, account_id, &name, kind),
    };
    state.runtime.set_buffer_remote_id(state, &buffer.id, &channel.id);
    if channel.is_personal() {
        let heading = if channel.kind == "G" { "Group Messages" } else { "Direct Messages" };
        state.runtime.set_buffer_category(state, &buffer.id, Some(heading), -channel.last_post_at / 1000);
    } else {
        state.runtime.set_buffer_group(state, &buffer.id, &rail_id(account_id, &channel.team_id));
        match &filed {
            Some((heading, place)) => state.runtime.set_buffer_category(state, &buffer.id, Some(heading), *place),
            None => state.runtime.set_buffer_category(state, &buffer.id, Some("Channels"), 900_000),
        }
    }
    state.runtime.set_buffer_read_only(state, &buffer.id, read_only);
    state.runtime.set_notify_level(state, &buffer.id, level, level);
    let topic = if channel.header.is_empty() { &channel.purpose } else { &channel.header };
    state.runtime.set_buffer_topic(state, &buffer.id, topic);
    // A private channel is marked with a lock, as Mattermost's sidebar marks it; an archived one with a box.
    let icon = if channel.archived {
        Some("inventory_2")
    } else if channel.kind == "P" {
        Some("lock")
    } else {
        None
    };
    state.runtime.set_buffer_name_icon(state, &buffer.id, icon);
    if let Some(url) = avatar {
        state.runtime.set_buffer_avatar(state, &buffer.id, &url);
    }
    // The server's count of what is unread is the truth at load: a mention count of a muted channel only.
    if !(muted && unread.1 == 0) {
        state.runtime.set_buffer_unread(state, &buffer.id, unread.0, unread.1);
    } else {
        state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
    }
}

/// Reads everything the account can see and makes the rail and the conversations from it. Run on a connection that
/// could not be resumed.
pub async fn load(state: &AppState, handle: &Handle) {
    let account_id = &handle.account_id;
    let api = &handle.api;
    let own = handle.own_id();
    let teams: Vec<world::Team> = api.get("/api/v4/users/me/teams", &[]).await.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default().iter().filter_map(world::parse_team).collect();
    let team_members = api.get("/api/v4/users/me/teams/members", &[]).await.unwrap_or(Value::Null);
    let preferences = api.get("/api/v4/users/me/preferences", &[]).await.unwrap_or(Value::Null);
    let mut channels: Vec<Channel> = Vec::new();
    let mut memberships: Vec<Value> = Vec::new();
    let mut categories: Vec<(String, Vec<world::Category>)> = Vec::new();
    for team in &teams {
        let list = api.get(&format!("/api/v4/users/me/teams/{}/channels", team.id), &[("include_deleted", "true".into())]).await.unwrap_or(Value::Null);
        channels.extend(list.as_array().into_iter().flatten().filter_map(world::parse_channel));
        let members = api.get(&format!("/api/v4/users/me/teams/{}/channels/members", team.id), &[]).await.unwrap_or(Value::Null);
        memberships.extend(members.as_array().cloned().unwrap_or_default());
        if let Ok(c) = api.get(&format!("/api/v4/users/me/teams/{}/channels/categories", team.id), &[]).await {
            categories.push((team.id.clone(), world::parse_categories(&c)));
        }
    }
    // An account in no team still has its direct and group messages.
    if teams.is_empty() {
        if let Ok(list) = api.get("/api/v4/users/me/channels", &[]).await {
            channels.extend(list.as_array().into_iter().flatten().filter_map(world::parse_channel));
        }
        if let Ok(list) = api.get("/api/v4/users/me/channel_members", &[("per_page", "200".into())]).await {
            memberships.extend(list.as_array().cloned().unwrap_or_default());
        }
    }
    // An archived channel is kept only where the account was in it and it still has something to read: Mattermost's own
    // sidebar shows none of them, and they are reached by search. They are left out here too.
    channels.retain(|c| !c.archived);
    let mut seen = HashSet::new();
    channels.retain(|c| seen.insert(c.id.clone()));

    // The people the account talks to directly, and those of its groups, by id and by name.
    let others: Vec<String> = channels.iter().filter_map(|c| c.other(&own)).collect();
    let group_names: Vec<String> = channels.iter().filter(|c| c.kind == "G").flat_map(|c| c.display_name.split(',').map(|n| n.trim().to_string()).collect::<Vec<_>>()).filter(|n| !n.is_empty()).collect();
    let mut users: Vec<Value> = Vec::new();
    if !others.is_empty() {
        users.extend(api.post("/api/v4/users/ids", &json!(others)).await.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default());
    }
    if !group_names.is_empty() {
        users.extend(api.post("/api/v4/users/usernames", &json!(group_names)).await.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default());
    }

    // The account's roles, and what each permits.
    let mut role_names: HashSet<String> = HashSet::new();
    {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world.teams = teams.iter().map(|t| (t.id.clone(), t.clone())).collect();
        world.team_order = order_teams(&teams, &preferences);
        world.team_roles = team_members.as_array().into_iter().flatten().filter_map(|m| Some((m["team_id"].as_str()?.to_string(), m["roles"].as_str().unwrap_or("").split_whitespace().map(str::to_string).collect()))).collect();
        world.channels = channels.iter().map(|c| (c.id.clone(), c.clone())).collect();
        world.members = memberships.iter().filter_map(world::parse_membership).collect();
        world.categories = categories.into_iter().collect();
        apply_preferences(&mut world, &preferences);
        for u in &users {
            if let Some(user) = world::parse_user(u) {
                world.users.insert(user.id.clone(), user);
            }
        }
        role_names.extend(world.system_roles.iter().cloned());
        role_names.extend(world.team_roles.values().flatten().cloned());
        role_names.extend(world.members.values().flat_map(|m| m.roles.iter().cloned()));
    }
    if !role_names.is_empty() {
        let names: Vec<String> = role_names.into_iter().collect();
        if let Ok(roles) = api.post("/api/v4/roles/names", &json!(names)).await {
            let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            for role in roles.as_array().into_iter().flatten() {
                if let Some(name) = role["name"].as_str() {
                    world.role_permissions.insert(name.to_string(), role["permissions"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(str::to_string)).collect());
                }
            }
        }
    }
    register_all(state, handle);
    people::load_statuses(state, handle).await;
    super::emoji::load(state, handle).await;
    if handle.server.has_calls() {
        let (state, handle) = (state.clone(), handle.clone());
        tokio::spawn(async move { calls::load(&state, &handle).await });
    }
    tracing::info!("mattermost[{account_id}]: {} teams, {} conversations", teams.len(), channels.len());
}

/// The teams in the order the account keeps them (its `teams_order` preference), then the rest by name.
pub fn order_teams(teams: &[world::Team], preferences: &Value) -> Vec<String> {
    let kept: Vec<String> = preferences.as_array().into_iter().flatten().find(|p| p["category"].as_str() == Some("teams_order")).and_then(|p| p["value"].as_str()).map(|v| v.split(',').map(str::to_string).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let mut rest: Vec<&world::Team> = teams.iter().filter(|t| !kept.contains(&t.id)).collect();
    rest.sort_by_key(|t| t.display_name.to_lowercase());
    kept.into_iter().filter(|id| teams.iter().any(|t| t.id == *id)).chain(rest.into_iter().map(|t| t.id.clone())).collect()
}

/// What the account's preferences say that matters here: its saved posts, and its own choice of collapsed threads.
pub fn apply_preferences(world: &mut world::World, preferences: &Value) {
    for p in preferences.as_array().into_iter().flatten() {
        match (p["category"].as_str(), p["name"].as_str(), p["value"].as_str()) {
            (Some("flagged_post"), Some(post), Some(v)) => {
                if v == "true" {
                    world.flagged.insert(post.to_string());
                } else {
                    world.flagged.remove(post);
                }
            }
            (Some("display_settings"), Some("collapsed_reply_threads"), Some(v)) => world.collapsed_choice = Some(v == "on"),
            _ => {}
        }
    }
}

/// Tells the window what the server offers and how the account reads threads there.
pub fn tell_server(state: &AppState, handle: &Handle) {
    let mut about = server_facts(handle);
    about["accountId"] = json!(handle.account_id);
    state.events.emit("mattermostServer", about);
}

/// What the server offers and how the account reads threads there, as the window reads it.
pub fn server_facts(handle: &Handle) -> Value {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let teams: Vec<Value> = world.team_order.iter().filter_map(|t| world.teams.get(t)).map(|t| json!({ "id": t.id, "name": t.display_name })).collect();
    drop(world);
    json!({
        "version": handle.server.version,
        "siteName": handle.server.site_name,
        "collapsedThreads": handle.server.collapsed_threads,
        "collapsedOn": messages::collapsed(handle),
        "calls": handle.server.has_calls(),
        "customStatuses": handle.server.custom_statuses,
        "priority": handle.server.post_priority,
        "scheduled": handle.server.at_least(10, 3),
        "teams": teams,
    })
}

/// The rail and every conversation, from what the world holds; and anything the account no longer has, taken away.
pub fn register_all(state: &AppState, handle: &Handle) {
    let account_id = &handle.account_id;
    let (teams, channels, own_name) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let teams: Vec<world::Team> = world.team_order.iter().filter_map(|t| world.teams.get(t).cloned()).collect();
        let channels: Vec<Channel> = world.channels.values().filter(|c| c.team_id.is_empty() || world.teams.contains_key(&c.team_id)).cloned().collect();
        (teams, channels, world.name_of(&world.own_id, &handle.server.teammate_name))
    };
    state.runtime.set_own_identity(account_id, &own_name);
    tell_server(state, handle);
    for (position, team) in teams.iter().enumerate() {
        register_team(state, handle, team, position as i64);
    }
    for channel in &channels {
        register_channel(state, handle, channel);
    }
    // Gone since the last time: a team left, a channel left or archived.
    let present: HashSet<String> = channels.iter().map(|c| c.id.clone()).collect();
    for buffer in state.runtime.list_buffers() {
        if buffer.account_id == *account_id && buffer.remote_id.as_ref().is_some_and(|r| !present.contains(r)) {
            state.runtime.remove_buffer(state, &buffer.id);
        }
    }
    let rails: HashSet<String> = teams.iter().map(|t| rail_id(account_id, &t.id)).collect();
    for group in state.runtime.list_buffer_groups() {
        if group.account_id == *account_id && group.service == "mattermost" && !rails.contains(&group.id) {
            state.runtime.remove_buffer_group(state, &group.id);
        }
    }
}

/// A channel the account has just come to see (joined, added to, a new direct message): read and registered.
pub async fn fetch_channel(state: &AppState, handle: &Handle, channel_id: &str) {
    let Ok(v) = handle.api.get(&format!("/api/v4/channels/{channel_id}"), &[]).await else { return };
    let Some(channel) = world::parse_channel(&v) else { return };
    if let Ok(m) = handle.api.get(&format!("/api/v4/channels/{channel_id}/members/me"), &[]).await {
        if let Some((id, membership)) = world::parse_membership(&m) {
            handle.world.lock().unwrap_or_else(|e| e.into_inner()).members.insert(id, membership);
        }
    }
    // The people of a direct or group message, to name it by.
    let missing: Vec<String> = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        channel.other(&world.own_id).into_iter().filter(|o| !world.users.contains_key(o)).collect()
    };
    if !missing.is_empty() {
        if let Ok(list) = handle.api.post("/api/v4/users/ids", &json!(missing)).await {
            messages::absorb_users(handle, list.as_array().map(Vec::as_slice).unwrap_or(&[]));
        }
    }
    if channel.kind == "G" {
        let names: Vec<String> = channel.display_name.split(',').map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect();
        if let Ok(list) = handle.api.post("/api/v4/users/usernames", &json!(names)).await {
            messages::absorb_users(handle, list.as_array().map(Vec::as_slice).unwrap_or(&[]));
        }
    }
    handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.insert(channel.id.clone(), channel.clone());
    register_channel(state, handle, &channel);
}

/// A team's categories read again (one was made, changed or removed), and its channels filed again.
async fn refile(state: &AppState, handle: &Handle, team_id: &str) {
    let Ok(c) = handle.api.get(&format!("/api/v4/users/me/teams/{team_id}/channels/categories"), &[]).await else { return };
    let channels: Vec<Channel> = {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world.categories.insert(team_id.to_string(), world::parse_categories(&c));
        world.channels.values().filter(|ch| ch.team_id == team_id).cloned().collect()
    };
    for channel in &channels {
        register_channel(state, handle, channel);
    }
}

/// The teams read again (one joined or left), and everything with them.
async fn reload_teams(state: &AppState, handle: &Handle) {
    load(state, handle).await;
}

/// A JSON value the websocket sends as a string inside the event.
fn inner(data: &Value, key: &str) -> Value {
    match &data[key] {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        other => other.clone(),
    }
}

fn typing(state: &AppState, handle: &Handle, channel: &str, user: &str) {
    let (own, name) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        (world.own_id.clone(), world.name_of(user, &handle.server.teammate_name))
    };
    if user == own {
        return;
    }
    if let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) {
        state.events.emit("typing", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "nicks": [name], "expiresInMs": TYPING_FOR_MS }));
    }
}

fn read_elsewhere(state: &AppState, handle: &Handle, channel: &str) {
    if let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) {
        state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
        state.events.emit("bufferRead", json!({ "accountId": handle.account_id, "bufferId": buffer.id }));
    }
}

/// Runs something that asks the server, apart from the websocket's reading (which must never wait on a request).
fn later<F>(state: &AppState, handle: &Handle, work: impl FnOnce(AppState, Handle) -> F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(work(state.clone(), handle.clone()));
}

/// One event.
pub fn handle(state: &AppState, handle: &Handle, event: &Value) -> Outcome {
    let Some(kind) = event["event"].as_str() else { return Outcome::Carry };
    let data = &event["data"];
    let broadcast = &event["broadcast"];
    let account_id = &handle.account_id;
    let channel_of = || data["channel_id"].as_str().filter(|c| !c.is_empty()).or(broadcast["channel_id"].as_str()).unwrap_or("").to_string();
    match kind {
        "posted" | "ephemeral_message" => {
            let post = inner(data, "post");
            let known = post["user_id"].as_str().is_some_and(|u| handle.world.lock().unwrap_or_else(|e| e.into_inner()).users.contains_key(u));
            let channel = post["channel_id"].as_str().unwrap_or("").to_string();
            let unknown_channel = messages::buffer_of(state, account_id, &channel).is_none();
            if known && !unknown_channel {
                messages::record(state, handle, &post);
            } else {
                // Its author (or its channel: a direct message somebody just began) is learned first.
                later(state, handle, move |state, handle| async move {
                    if unknown_channel {
                        fetch_channel(&state, &handle, &channel).await;
                    }
                    messages::learn_authors(&handle, &[&post]).await;
                    messages::record(&state, &handle, &post);
                });
            }
            messages::count_reply(state, handle, &inner(data, "post"));
            // A new post moves a direct message to the top.
            if let Some(c) = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get_mut(&channel_of()) {
                c.last_post_at = chrono::Utc::now().timestamp_millis();
            }
        }
        "post_edited" => messages::update(state, handle, &inner(data, "post")),
        "post_deleted" => {
            let post = inner(data, "post");
            if let (Some(id), Some(channel)) = (post["id"].as_str(), post["channel_id"].as_str()) {
                if let Some(buffer) = messages::buffer_of(state, account_id, channel) {
                    state.runtime.delete_message(state, &buffer.id, id);
                }
            }
        }
        "reaction_added" | "reaction_removed" => {
            let reaction = inner(data, "reaction");
            let channel = reaction["channel_id"].as_str().map(str::to_string).unwrap_or_else(channel_of);
            if let (Some(post), Some(name), Some(buffer)) = (reaction["post_id"].as_str(), reaction["emoji_name"].as_str(), messages::buffer_of(state, account_id, &channel)) {
                let me = reaction["user_id"].as_str() == Some(handle.own_id().as_str());
                let shown = super::emoji::shown(handle, name);
                state.runtime.update_reaction(state, &buffer.id, post, &shown, me, kind == "reaction_added");
            }
        }
        "typing" => {
            if let Some(user) = data["user_id"].as_str() {
                typing(state, handle, &channel_of(), user);
            }
        }
        "channel_viewed" => read_elsewhere(state, handle, &channel_of()),
        "multiple_channels_viewed" => {
            for channel in data["channel_times"].as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                read_elsewhere(state, handle, &channel);
            }
        }
        "channel_created" | "channel_restored" | "direct_added" | "group_added" | "channel_converted" => {
            let channel = channel_of();
            if !channel.is_empty() {
                later(state, handle, move |state, handle| async move { fetch_channel(&state, &handle, &channel).await });
            }
        }
        "channel_updated" => {
            let updated = inner(data, "channel");
            if let Some(channel) = world::parse_channel(&updated) {
                let known = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.insert(channel.id.clone(), channel.clone()).is_some();
                if known {
                    register_channel(state, handle, &channel);
                }
            }
        }
        "channel_deleted" => {
            let channel = channel_of();
            let removed = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.remove(&channel);
            if removed.is_some() {
                if let Some(buffer) = messages::buffer_of(state, account_id, &channel) {
                    state.runtime.remove_buffer(state, &buffer.id);
                }
            }
        }
        "channel_member_updated" => {
            let member = inner(data, "channelMember");
            if member["user_id"].as_str() == Some(handle.own_id().as_str()) {
                if let Some((id, membership)) = world::parse_membership(&member) {
                    let channel = {
                        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                        world.members.insert(id.clone(), membership);
                        world.channels.get(&id).cloned()
                    };
                    if let Some(channel) = channel {
                        register_channel(state, handle, &channel);
                    }
                }
            }
        }
        "user_added" => {
            let channel = channel_of();
            if data["user_id"].as_str() == Some(handle.own_id().as_str()) {
                later(state, handle, move |state, handle| async move { fetch_channel(&state, &handle, &channel).await });
            } else if let Some(buffer) = messages::buffer_of(state, account_id, &channel) {
                later(state, handle, move |state, handle| async move {
                    let _ = people::refresh_members(&state, &handle, &buffer).await;
                });
            }
        }
        "user_removed" => {
            let channel = channel_of();
            if data["user_id"].as_str() == Some(handle.own_id().as_str()) || broadcast["user_id"].as_str() == Some(handle.own_id().as_str()) {
                handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.remove(&channel);
                if let Some(buffer) = messages::buffer_of(state, account_id, &channel) {
                    state.runtime.remove_buffer(state, &buffer.id);
                }
            } else if let Some(buffer) = messages::buffer_of(state, account_id, &channel) {
                later(state, handle, move |state, handle| async move {
                    let _ = people::refresh_members(&state, &handle, &buffer).await;
                });
            }
        }
        "added_to_team" | "leave_team" | "update_team" | "delete_team" | "restore_team" => {
            later(state, handle, |state, handle| async move { reload_teams(&state, &handle).await });
        }
        "sidebar_category_created" | "sidebar_category_updated" | "sidebar_category_deleted" | "sidebar_category_order_updated" => {
            let team = broadcast["team_id"].as_str().or(data["team_id"].as_str()).unwrap_or("").to_string();
            if !team.is_empty() {
                later(state, handle, move |state, handle| async move { refile(&state, &handle, &team).await });
            }
        }
        "status_change" => {
            if let (Some(user), Some(status)) = (data["user_id"].as_str(), data["status"].as_str()) {
                people::status_changed(state, handle, user, status);
            }
        }
        "user_updated" => {
            if let Some(user) = world::parse_user(&data["user"]) {
                let id = user.id.clone();
                {
                    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                    let status = world.users.get(&id).map(|u| u.status.clone()).unwrap_or_default();
                    world.users.insert(id.clone(), world::User { status, ..user });
                }
                people::status_changed(state, handle, &id, "");
            }
        }
        // A role's permissions changed (a channel made read-only, a team's rights changed): every conversation is told
        // again whether it may be written in.
        "role_updated" => {
            let role = inner(data, "role");
            if let Some(name) = role["name"].as_str() {
                let held = {
                    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                    let held = world.role_permissions.contains_key(name);
                    if held {
                        world.role_permissions.insert(name.to_string(), role["permissions"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(str::to_string)).collect());
                    }
                    held
                };
                if held {
                    register_all(state, handle);
                }
            }
        }
        // A channel given a scheme of its own (its moderation): its membership's roles are read again.
        "channel_scheme_updated" => {
            let channel = channel_of();
            later(state, handle, move |state, handle| async move { fetch_channel(&state, &handle, &channel).await });
        }
        "emoji_added" => {
            let e = inner(data, "emoji");
            if let (Some(name), Some(id)) = (e["name"].as_str(), e["id"].as_str()) {
                handle.world.lock().unwrap_or_else(|e| e.into_inner()).emoji.insert(name.to_string(), id.to_string());
                super::emoji::publish(state, handle);
            }
        }
        "preference_changed" | "preferences_changed" | "preferences_deleted" => {
            let mut list = inner(data, "preferences");
            if list.is_null() {
                list = json!([inner(data, "preference")]);
            }
            if kind == "preferences_deleted" {
                for p in list.as_array_mut().into_iter().flatten() {
                    p["value"] = json!("false");
                }
            }
            let before = handle.world.lock().unwrap_or_else(|e| e.into_inner()).collapsed_choice;
            apply_preferences(&mut handle.world.lock().unwrap_or_else(|e| e.into_inner()), &list);
            if handle.world.lock().unwrap_or_else(|e| e.into_inner()).collapsed_choice != before {
                register_all(state, handle);
            }
            super::extra::saved_changed(state, handle);
        }
        "thread_updated" | "thread_read_changed" | "thread_follow_changed" => super::extra::thread_event(state, handle, kind, data),
        "draft_created" | "draft_updated" | "draft_deleted" => super::extra::draft_event(state, handle, kind, data),
        "channel_bookmark_created" | "channel_bookmark_updated" | "channel_bookmark_deleted" | "channel_bookmark_sorted" => super::extra::bookmark_event(state, handle, &channel_of()),
        "post_acknowledgement_added" | "post_acknowledgement_removed" => {
            let ack = inner(data, "acknowledgement");
            if let Some(post) = ack["post_id"].as_str().map(str::to_string) {
                later(state, handle, move |state, handle| async move {
                    if let Ok(p) = handle.api.get(&format!("/api/v4/posts/{post}"), &[]).await {
                        messages::update(&state, &handle, &p);
                    }
                });
            }
        }
        other if other.starts_with("custom_com.mattermost.calls_") => calls::event(state, handle, other, data, broadcast),
        _ => {}
    }
    Outcome::Carry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teams_are_in_the_accounts_order_and_the_rest_by_name() {
        let t = |id: &str, name: &str| world::Team { id: id.into(), display_name: name.into(), ..Default::default() };
        let teams = vec![t("a", "Zed"), t("b", "Alpha"), t("c", "Mid")];
        let prefs = json!([{ "category": "teams_order", "name": "", "value": "c,gone" }]);
        assert_eq!(order_teams(&teams, &prefs), vec!["c", "b", "a"]);
        assert_eq!(order_teams(&teams, &Value::Null), vec!["b", "c", "a"]);
    }

    #[test]
    fn preferences_say_which_posts_are_saved_and_whether_threads_are_collapsed() {
        let mut w = world::World::default();
        apply_preferences(&mut w, &json!([{ "category": "flagged_post", "name": "p1", "value": "true" }, { "category": "display_settings", "name": "collapsed_reply_threads", "value": "off" }]));
        assert!(w.flagged.contains("p1"));
        assert_eq!(w.collapsed_choice, Some(false));
        apply_preferences(&mut w, &json!([{ "category": "flagged_post", "name": "p1", "value": "false" }]));
        assert!(w.flagged.is_empty());
    }

    #[test]
    fn a_value_sent_as_a_string_inside_an_event_is_read() {
        assert_eq!(inner(&json!({ "post": "{\"id\":\"x\"}" }), "post")["id"], "x");
        assert_eq!(inner(&json!({ "user": { "id": "y" } }), "user")["id"], "y");
        assert!(inner(&json!({}), "post").is_null());
    }

    #[test]
    fn a_slash_in_a_name_does_not_divide_a_conversations_name() {
        assert_eq!(unslashed("R/D"), "R∕D");
    }
}
