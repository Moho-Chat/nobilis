//! People: who is in a channel and who is online, the card for a person, the account's own status and profile, and a
//! direct message opened with somebody.
//!
//! Mattermost tells a client of its own status changes as they happen (`status_change`), and of nobody else's: those it
//! answers for many people at once (`/users/status/ids`), and its own app asks once a minute for the people it shows.
//! So does this: the people of the member list the window opened last, and those of every direct and group message.
//! A status is `online`, `away`, `dnd` or `offline`; somebody may put an emoji and a few words beside it for a while (a
//! custom status).

use super::world::User;
use super::{emoji, events, media, messages, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// A status as the window's words have it.
pub fn status_word(status: &str) -> &'static str {
    match status {
        "online" => "online",
        "away" => "idle",
        "dnd" => "dnd",
        _ => "offline",
    }
}

/// The window's status as Mattermost's.
pub fn wire_status(status: &str) -> Option<&'static str> {
    match status {
        "online" => Some("online"),
        "idle" | "away" => Some("away"),
        "dnd" => Some("dnd"),
        "offline" | "invisible" => Some("offline"),
        _ => None,
    }
}

fn activity(handle: &Handle, user: &User) -> Value {
    match &user.custom_status {
        Some((emoji_name, text)) => {
            let shown = if emoji_name.is_empty() { String::new() } else { emoji::shown(handle, emoji_name) };
            let shown = if shown.starts_with("[mattermost:emoji:") { format!(":{emoji_name}:") } else { shown };
            json!({ "kind": "custom", "text": format!("{shown} {text}").trim() })
        }
        None => Value::Null,
    }
}

/// One person as a member-list entry.
pub fn entry(handle: &Handle, user: &User, admin: bool) -> Value {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let status = status_word(&user.status);
    json!({
        "nick": world.name_of(&user.id, &handle.server.teammate_name),
        "username": user.username,
        "userId": user.id,
        "prefix": if admin { "@" } else { "" },
        "away": status == "offline",
        "status": status,
        "activity": activity(handle, user),
        "bot": user.bot,
        "avatarUrl": media::avatar(&handle.account_id, &user.id, user.picture),
        "roles": if admin { vec!["Channel Admin"] } else { Vec::<&str>::new() },
    })
}

/// The most people drawn in one list.
const LISTED: usize = 1000;

/// Lists the people of a channel and tells the window.
pub async fn refresh_members(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<usize> {
    let channel = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Mattermost conversation"))?;
    let mut members: Vec<Value> = Vec::new();
    for page in 0..(LISTED / 200) {
        let list = handle.api.get(&format!("/api/v4/channels/{channel}/members"), &[("page", page.to_string()), ("per_page", "200".into())]).await.map_err(|e| anyhow!("{e}"))?;
        let got = list.as_array().cloned().unwrap_or_default();
        let done = got.len() < 200;
        members.extend(got);
        if done {
            break;
        }
    }
    let ids: Vec<String> = members.iter().filter_map(|m| m["user_id"].as_str().map(str::to_string)).collect();
    let missing: Vec<String> = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        ids.iter().filter(|id| !world.users.contains_key(*id)).cloned().collect()
    };
    for chunk in missing.chunks(200) {
        if let Ok(list) = handle.api.post("/api/v4/users/ids", &json!(chunk)).await {
            messages::absorb_users(handle, list.as_array().map(Vec::as_slice).unwrap_or(&[]));
        }
    }
    statuses_of(handle, &ids).await;
    let total = match handle.api.get(&format!("/api/v4/channels/{channel}/stats"), &[]).await {
        Ok(stats) => stats["member_count"].as_u64().map(|n| n as usize).unwrap_or(ids.len()),
        Err(_) => ids.len(),
    };
    let users: Vec<(User, bool)> = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        members.iter().filter_map(|m| Some((world.users.get(m["user_id"].as_str()?)?.clone(), m["scheme_admin"].as_bool().unwrap_or(false)))).filter(|(u, _)| !u.deleted).collect()
    };
    let list = Value::Array(users.iter().map(|(u, admin)| entry(handle, u, *admin)).collect());
    state.runtime.set_presence(&buffer.id, list.clone());
    state.runtime.set_presence_total(&buffer.id, total);
    state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": list, "total": total }));
    Ok(total)
}

/// Asks for many people's status at once and keeps it; says whose differs from what was kept, with the new one.
async fn statuses_of(handle: &Handle, ids: &[String]) -> Vec<(String, String)> {
    let mut changed = Vec::new();
    for chunk in ids.chunks(500) {
        if let Ok(list) = handle.api.post("/api/v4/users/status/ids", &json!(chunk)).await {
            let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            for s in list.as_array().into_iter().flatten() {
                if let (Some(id), Some(status)) = (s["user_id"].as_str(), s["status"].as_str()) {
                    if let Some(u) = world.users.get_mut(id) {
                        if u.status != status {
                            changed.push((id.to_string(), status.to_string()));
                        }
                        u.status = status.to_string();
                    }
                }
            }
        }
    }
    changed
}

/// The window opened this conversation's member list: its people are the ones whose statuses are kept up.
pub fn watch_members(handle: &Handle, buffer_id: &str) {
    handle.world.lock().unwrap_or_else(|e| e.into_inner()).watched = Some(buffer_id.to_string());
}

/// The people whose statuses are asked after every minute: those of the member list opened last, and those in the
/// account's direct and group messages (the sidebar shows theirs).
fn shown_people(state: &AppState, handle: &Handle) -> Vec<String> {
    let own = handle.own_id();
    let (watched, mut ids): (Option<String>, Vec<String>) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let partners = world.channels.values().filter_map(|c| c.other(&own)).collect();
        (world.watched.clone(), partners)
    };
    if let Some(list) = watched.and_then(|b| state.runtime.get_presence(&b)) {
        ids.extend(list.as_array().into_iter().flatten().filter_map(|m| m["userId"].as_str().map(str::to_string)));
    }
    ids.retain(|id| *id != own);
    ids.sort();
    ids.dedup();
    ids
}

/// Asks after the statuses the window shows, and tells it of those that changed.
pub async fn poll_statuses(state: &AppState, handle: &Handle) {
    let ids = shown_people(state, handle);
    if ids.is_empty() {
        return;
    }
    for (id, status) in statuses_of(handle, &ids).await {
        status_changed(state, handle, &id, &status);
    }
}

/// The status of everybody the account knows, at load; and its own, which the server keeps.
pub async fn load_statuses(state: &AppState, handle: &Handle) {
    let ids: Vec<String> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).users.keys().cloned().collect();
    statuses_of(handle, &ids).await;
    let own = handle.own_id();
    let (status, custom) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world.users.get(&own).map(|u| (u.status.clone(), u.custom_status.clone())).unwrap_or_default()
    };
    // How the account stands on the server is the truth, set from another app as well as from here.
    if !status.is_empty() {
        state.runtime.set_account_status(&handle.account_id, status_word(&status));
    }
    state.runtime.set_account_status_text(&handle.account_id, &custom.map(|(_, t)| t).unwrap_or_default());
    for id in ids {
        if id != own {
            status_changed(state, handle, &id, "");
        }
    }
}

/// Somebody's status changed (or their profile did, with `status` empty): wherever they are listed, so they are now.
pub fn status_changed(state: &AppState, handle: &Handle, user_id: &str, status: &str) {
    let user = {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(u) = world.users.get_mut(user_id) {
            if !status.is_empty() {
                u.status = status.to_string();
            }
        }
        world.users.get(user_id).cloned()
    };
    let Some(user) = user else { return };
    if user_id == handle.own_id() && !status.is_empty() {
        state.runtime.set_account_status(&handle.account_id, status_word(status));
    }
    for buffer in state.runtime.list_buffers() {
        if buffer.account_id != handle.account_id {
            continue;
        }
        let Some(existing) = state.runtime.get_presence(&buffer.id) else { continue };
        let Some(members) = existing.as_array() else { continue };
        if !members.iter().any(|m| m["userId"].as_str() == Some(user_id)) {
            continue;
        }
        let updated: Vec<Value> = members
            .iter()
            .map(|m| {
                if m["userId"].as_str() != Some(user_id) {
                    return m.clone();
                }
                let mut m = m.clone();
                let word = status_word(&user.status);
                m["status"] = json!(word);
                m["away"] = json!(word == "offline");
                m["activity"] = activity(handle, &user);
                m
            })
            .collect();
        let list = json!(updated);
        state.runtime.set_presence(&buffer.id, list.clone());
        state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": list }));
    }
}

/// Sets the account's own status (and, with `text`, its custom status; an empty text clears it).
pub async fn apply_status(handle: &Handle, status: &str, text: Option<&str>) -> Result<()> {
    let own = handle.own_id();
    if let Some(wire) = wire_status(status) {
        handle.api.put(&format!("/api/v4/users/{own}/status"), &json!({ "user_id": own, "status": wire })).await.map_err(|e| anyhow!("{e}"))?;
    }
    if let Some(text) = text {
        if text.trim().is_empty() {
            handle.api.delete(&format!("/api/v4/users/{own}/status/custom")).await.map_err(|e| anyhow!("{e}"))?;
        } else {
            set_custom_status(handle, "speech_balloon", text, None).await?;
        }
    }
    Ok(())
}

/// Sets the account's custom status: an emoji's name, the words, and when it ends (RFC 3339) or never.
pub async fn set_custom_status(handle: &Handle, emoji_name: &str, text: &str, until: Option<&str>) -> Result<()> {
    if !handle.server.custom_statuses {
        return Err(anyhow!("This server has custom statuses turned off"));
    }
    let own = handle.own_id();
    let mut body = json!({ "emoji": if emoji_name.is_empty() { "speech_balloon" } else { emoji_name }, "text": text.chars().take(100).collect::<String>() });
    match until {
        Some(t) => {
            body["duration"] = json!("date_and_time");
            body["expires_at"] = json!(t);
        }
        None => body["duration"] = json!(""),
    }
    handle.api.put(&format!("/api/v4/users/{own}/status/custom"), &body).await.map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

/// The person a card is for: the other person of a direct message, else the id the window gave.
fn target_of(state: &AppState, handle: &Handle, buffer_id: &str, user_id: &str) -> Option<String> {
    if super::is_id(user_id) {
        return Some(user_id.to_string());
    }
    let buffer = state.runtime.get_buffer(buffer_id)?;
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(id) = world.user_by_name(user_id) {
        return Some(id);
    }
    world.channels.get(buffer.remote_id.as_deref()?)?.other(&world.own_id)
}

/// The card for a person: their names, picture, status, position, local time, and how to reach them.
pub async fn profile(state: &AppState, account_id: &str, buffer_id: &str, who: &str, user_id: &str) -> Value {
    let mut card = crate::profile::pending("mattermost", account_id, who);
    card["pending"] = json!(false);
    let Some(handle) = state.runtime.mattermost_handle(account_id) else { return card };
    let Some(target) = target_of(state, &handle, buffer_id, user_id) else { return card };
    card["id"] = json!(target);
    let (user_path, status_path) = (format!("/api/v4/users/{target}"), format!("/api/v4/users/{target}/status"));
    let (user, status) = tokio::join!(handle.api.get(&user_path, &[]), handle.api.get(&status_path, &[]));
    let Ok(user) = user else { return card };
    let Some(parsed) = super::world::parse_user(&user) else { return card };
    {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let status = status.as_ref().ok().and_then(|s| s["status"].as_str()).map(str::to_string).or_else(|| world.users.get(&target).map(|u| u.status.clone())).unwrap_or_default();
        world.users.insert(target.clone(), User { status, ..parsed.clone() });
    }
    let known = handle.world.lock().unwrap_or_else(|e| e.into_inner()).users.get(&target).cloned().unwrap_or(parsed);
    let name = handle.world.lock().unwrap_or_else(|e| e.into_inner()).name_of(&target, &handle.server.teammate_name);
    card["name"] = json!(name);
    card["handle"] = json!(format!("@{}", known.username));
    card["status"] = json!(status_word(&known.status));
    crate::profile::set(&mut card, "avatarUrl", Some(json!(media::avatar(account_id, &target, known.picture))));
    crate::profile::set(&mut card, "activity", Some(activity(&handle, &known)).filter(|a| !a.is_null()));
    if let Some(ms) = user["create_at"].as_i64() {
        card["createdTs"] = json!(ms / 1000);
    }
    let full = known.full_name();
    if !full.is_empty() && full != name {
        crate::profile::note(&mut card, "Name", full);
    }
    if !known.nickname.is_empty() && known.nickname != name {
        crate::profile::note(&mut card, "Nickname", known.nickname.clone());
    }
    if !known.position.is_empty() {
        crate::profile::note(&mut card, "Position", known.position.clone());
    }
    if let Some(email) = user["email"].as_str().filter(|e| !e.is_empty()) {
        crate::profile::note(&mut card, "Email", email);
    }
    // Where they are, as they have said it.
    let zone = &user["timezone"];
    let zone_name = if zone["useAutomaticTimezone"].as_str() == Some("true") { zone["automaticTimezone"].as_str() } else { zone["manualTimezone"].as_str() };
    if let Some(zone_name) = zone_name.filter(|z| !z.is_empty()) {
        crate::profile::note(&mut card, "Time zone", zone_name.replace('_', " "));
    }
    if known.bot {
        crate::profile::note(&mut card, "Account", "A bot");
    }
    if known.deleted {
        crate::profile::note(&mut card, "Account", "Deactivated");
    }
    if known.roles.split_whitespace().any(|r| r == "system_admin") {
        crate::profile::note(&mut card, "Role", "System Admin");
    }
    card
}

/// Opens (or makes) the direct message with somebody, and answers its buffer.
pub async fn open_dm(state: &AppState, handle: &Handle, user: &str) -> Result<String> {
    let target = if super::is_id(user) {
        user.to_string()
    } else {
        let found = handle.api.get(&format!("/api/v4/users/username/{}", user.trim_start_matches('@')), &[]).await.map_err(|e| anyhow!("{e}"))?;
        found["id"].as_str().ok_or_else(|| anyhow!("nobody on this server is called that"))?.to_string()
    };
    let own = handle.own_id();
    let channel = handle.api.post("/api/v4/channels/direct", &json!([own, target])).await.map_err(|e| anyhow!("{e}"))?;
    let id = channel["id"].as_str().ok_or_else(|| anyhow!("the server did not say where that conversation is"))?.to_string();
    events::fetch_channel(state, handle, &id).await;
    messages::buffer_of(state, &handle.account_id, &id).map(|b| b.id).ok_or_else(|| anyhow!("the conversation did not come up"))
}

/// Opens (or makes) a group message with these people (usernames or ids), and answers its buffer.
pub async fn open_group(state: &AppState, handle: &Handle, people: &[String]) -> Result<String> {
    let mut ids = vec![handle.own_id()];
    for p in people {
        if super::is_id(p) {
            ids.push(p.clone());
        } else {
            let found = handle.api.get(&format!("/api/v4/users/username/{}", p.trim_start_matches('@')), &[]).await.map_err(|e| anyhow!("{e}"))?;
            ids.push(found["id"].as_str().ok_or_else(|| anyhow!("nobody on this server is called {p}"))?.to_string());
        }
    }
    ids.sort();
    ids.dedup();
    if ids.len() < 3 {
        return Err(anyhow!("a group message is three people or more: name at least two others"));
    }
    let channel = handle.api.post("/api/v4/channels/group", &json!(ids)).await.map_err(|e| anyhow!("{e}"))?;
    let id = channel["id"].as_str().ok_or_else(|| anyhow!("the server did not say where that conversation is"))?.to_string();
    events::fetch_channel(state, handle, &id).await;
    messages::buffer_of(state, &handle.account_id, &id).map(|b| b.id).ok_or_else(|| anyhow!("the conversation did not come up"))
}

/// The account's own profile, as the settings page shows it.
pub async fn own_profile(handle: &Handle) -> Result<Value> {
    let me = handle.api.get("/api/v4/users/me", &[]).await.map_err(|e| anyhow!("{e}"))?;
    let own = handle.own_id();
    let custom = handle.api.get(&format!("/api/v4/users/{own}/status"), &[]).await.ok();
    let parsed = super::world::parse_user(&me);
    Ok(json!({
        "username": me["username"],
        "firstName": me["first_name"],
        "lastName": me["last_name"],
        "nickname": me["nickname"],
        "position": me["position"],
        "email": me["email"],
        "avatarUrl": media::avatar(&handle.account_id, &own, me["last_picture_update"].as_i64().unwrap_or(0)),
        "status": custom.as_ref().and_then(|s| s["status"].as_str()).map(status_word),
        "customStatus": parsed.and_then(|p| p.custom_status).map(|(e, t)| json!({ "emoji": e, "text": t })),
        "customStatuses": handle.server.custom_statuses,
        "server": handle.api.base(),
        // What notifies the account by default, on every app: desktop `all`/`mention`/`none`, and the same for phones.
        "desktop": me["notify_props"]["desktop"],
        "push": me["notify_props"]["push"],
    }))
}

/// Changes the account's own profile: what is given is changed, the rest left.
pub async fn set_profile(handle: &Handle, fields: &Value) -> Result<()> {
    let mut patch = serde_json::Map::new();
    for (from, to) in [("firstName", "first_name"), ("lastName", "last_name"), ("nickname", "nickname"), ("position", "position")] {
        if let Some(v) = fields[from].as_str() {
            patch.insert(to.to_string(), json!(v));
        }
    }
    // The account's default notifications, kept with its other notify props (which a patch replaces whole).
    if fields["desktop"].is_string() || fields["push"].is_string() {
        let me = handle.api.get("/api/v4/users/me", &[]).await.map_err(|e| anyhow!("{e}"))?;
        let mut notify = me["notify_props"].clone();
        for key in ["desktop", "push"] {
            if let Some(v) = fields[key].as_str().filter(|v| matches!(*v, "all" | "mention" | "none")) {
                notify[key] = json!(v);
            }
        }
        patch.insert("notify_props".to_string(), notify);
    }
    if patch.is_empty() {
        return Ok(());
    }
    handle.api.put("/api/v4/users/me/patch", &Value::Object(patch)).await.map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

/// Sets the account's own picture from a file.
pub async fn set_avatar(handle: &Handle, path: &str) -> Result<()> {
    let bytes = tokio::fs::read(path).await.map_err(|e| anyhow!("reading {path}: {e}"))?;
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "avatar.png".into());
    let own = handle.own_id();
    handle.api.upload_image(&format!("/api/v4/users/{own}/image"), &name, super::send::content_type_of(&name), bytes).await.map_err(|e| anyhow!("{e}"))
}

/// The account's sessions: where it is signed in.
pub async fn sessions(handle: &Handle) -> Result<Value> {
    let own = handle.own_id();
    let list = handle.api.get(&format!("/api/v4/users/{own}/sessions"), &[]).await.map_err(|e| anyhow!("{e}"))?;
    let own_session = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_session.clone();
    let out: Vec<Value> = list
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            let props = &s["props"];
            let this = !own_session.is_empty() && s["id"].as_str() == Some(own_session.as_str());
            let what = [props["platform"].as_str(), props["os"].as_str(), props["browser"].as_str()].into_iter().flatten().filter(|t| !t.is_empty() && !t.starts_with("Unknown")).collect::<Vec<_>>().join(", ");
            let what = if this { "moho (this one)".to_string() } else if what.is_empty() { "Unknown".to_string() } else { what };
            json!({
                "id": s["id"],
                "what": what,
                "lastActive": s["last_activity_at"].as_i64().map(|ms| ms / 1000),
                "created": s["create_at"].as_i64().map(|ms| ms / 1000),
                "this": this,
                "kind": if s["is_oauth"].as_bool() == Some(true) { "OAuth" } else if props["type"].as_str() == Some("UserAccessToken") { "Access token" } else { "Session" },
            })
        })
        .collect();
    Ok(json!({ "sessions": out }))
}

/// Ends one of the account's sessions.
pub async fn revoke_session(handle: &Handle, session_id: &str) -> Result<()> {
    let own = handle.own_id();
    handle.api.post(&format!("/api/v4/users/{own}/sessions/revoke"), &json!({ "session_id": session_id })).await.map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

/// Names typed after `@` and who they might be, for the message box.
pub async fn autocomplete(handle: &Handle, channel_id: &str, team_id: &str, typed: &str) -> Result<Value> {
    let mut query = vec![("name", typed.to_string()), ("limit", "25".to_string())];
    if !channel_id.is_empty() {
        query.push(("in_channel", channel_id.to_string()));
    }
    if !team_id.is_empty() {
        query.push(("in_team", team_id.to_string()));
    }
    let found = handle.api.get("/api/v4/users/autocomplete", &query).await.map_err(|e| anyhow!("{e}"))?;
    let mut users: Vec<Value> = found["users"].as_array().cloned().unwrap_or_default();
    users.extend(found["out_of_channel"].as_array().cloned().unwrap_or_default());
    Ok(json!(users
        .iter()
        .map(|u| json!({ "username": u["username"], "id": u["id"], "name": format!("{} {}", u["first_name"].as_str().unwrap_or(""), u["last_name"].as_str().unwrap_or("")).trim(), "avatarUrl": media::avatar(&handle.account_id, u["id"].as_str().unwrap_or(""), u["last_picture_update"].as_i64().unwrap_or(0)) }))
        .collect::<Vec<_>>()))
}

/// A channel's notification level as the account keeps it (its membership's `notify_props`): muted where only mentions
/// make it unread, else what its desktop notifications are set to.
pub fn level_of(notify: &Value) -> crate::notify::NotifyLevel {
    use crate::notify::NotifyLevel;
    if notify["mark_unread"].as_str() == Some("mention") {
        return NotifyLevel::Mute;
    }
    match notify["desktop"].as_str() {
        Some("all") => NotifyLevel::All,
        Some("mention") => NotifyLevel::Mentions,
        Some("none") => NotifyLevel::Mute,
        _ => NotifyLevel::Default,
    }
}

/// The `notify_props` that say a level.
pub fn props_for(level: crate::notify::NotifyLevel) -> Value {
    use crate::notify::NotifyLevel;
    match level {
        NotifyLevel::Mute => json!({ "mark_unread": "mention", "desktop": "default", "push": "default" }),
        NotifyLevel::All => json!({ "mark_unread": "all", "desktop": "all", "push": "all" }),
        NotifyLevel::Mentions | NotifyLevel::Direct => json!({ "mark_unread": "all", "desktop": "mention", "push": "mention" }),
        NotifyLevel::Default => json!({ "mark_unread": "all", "desktop": "default", "push": "default" }),
    }
}

/// Sets a channel's notification level on the account, so the account's phone and other apps keep it too.
pub async fn set_level(state: &AppState, handle: &Handle, buffer: &Buffer, level: crate::notify::NotifyLevel) -> Result<()> {
    let channel = buffer.remote_id.clone().ok_or_else(|| anyhow!("this is not a Mattermost conversation"))?;
    let own = handle.own_id();
    let mut props = props_for(level);
    props["channel_id"] = json!(channel);
    props["user_id"] = json!(own);
    handle.api.put(&format!("/api/v4/channels/{channel}/members/{own}/notify_props"), &props).await.map_err(|e| anyhow!("{e}"))?;
    let updated = {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = world.members.get_mut(&channel) {
            m.mark_unread = props["mark_unread"].as_str().unwrap_or("all").to_string();
            for (k, v) in props.as_object().into_iter().flatten() {
                m.notify[k] = v.clone();
            }
        }
        world.channels.get(&channel).cloned()
    };
    if let Some(c) = updated {
        events::register_channel(state, handle, &c);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_is_read_from_the_membership_and_written_back_the_same() {
        use crate::notify::NotifyLevel;
        for level in [NotifyLevel::Mute, NotifyLevel::All, NotifyLevel::Mentions, NotifyLevel::Default] {
            assert_eq!(level_of(&props_for(level)), level);
        }
        assert_eq!(level_of(&json!({ "desktop": "none" })), NotifyLevel::Mute);
        assert_eq!(level_of(&Value::Null), NotifyLevel::Default);
    }

    #[test]
    fn statuses_go_both_ways() {
        assert_eq!(status_word("away"), "idle");
        assert_eq!(status_word("dnd"), "dnd");
        assert_eq!(status_word("whatever"), "offline");
        assert_eq!(wire_status("idle"), Some("away"));
        assert_eq!(wire_status("invisible"), Some("offline"));
        assert_eq!(wire_status("focus"), None);
    }
}
