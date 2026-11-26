//! Who is here: the list beside a conversation, and who is online.
//!
//! A guild's list is Fluxer's own (`gateway/events.md`, Guild Member List Update): the client asks for a window of a
//! channel's list with a Lazy Request (`member_list_channels`), and Fluxer sends the groups (hoisted roles, online,
//! offline) and the members in them, each with its presence, and sends the window again whenever it changes. Only the
//! channel whose list is on screen is asked for, as Fluxer's own app does. A direct message or a group is its
//! recipients, with the presences Fluxer sends for friends and group members.

use super::media;
use super::world::{Kind, World};
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// The window of a list that is asked for: the first hundred, which is one range of Fluxer's.
const WINDOW: [u32; 2] = [0, 99];

/// A status as the window's words have it.
pub fn status_word(status: &str) -> &'static str {
    match status {
        "online" => "online",
        "idle" => "idle",
        "dnd" => "dnd",
        _ => "offline",
    }
}

/// One person as a member-list entry.
pub fn entry(world: &World, handle: &Handle, guild: Option<&str>, user_id: &str, member: Option<&Value>, status: &str, custom: Option<&str>) -> Value {
    let user = world.users.get(user_id).cloned().unwrap_or_default();
    let nick = member.and_then(|m| m["nick"].as_str()).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| world.name_in(guild, user_id));
    let avatar = user.avatar.as_deref().and_then(|h| media::asset_url(&handle.instance, "avatars", user_id, h)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, Some("image/webp")));
    // The roles held, highest first; the highest one listed apart ("hoisted") is the heading the person is listed
    // under, and the highest with a colour is the colour of their name.
    let held: Vec<super::world::Role> = match (member, guild.and_then(|g| world.guilds.get(g))) {
        (Some(m), Some(g)) => {
            let mut held: Vec<super::world::Role> = m["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str()).filter_map(|r| g.roles.get(r).cloned()).collect();
            held.sort_by_key(|role| -role.position);
            held
        }
        _ => Vec::new(),
    };
    let roles: Vec<String> = held.iter().map(|r| r.name.clone()).collect();
    let hoisted = held.iter().find(|r| r.hoist);
    let colour = held.iter().find(|r| r.color != 0).map(|r| format!("#{:06x}", r.color));
    // From the member as given, or as kept: a timeout that has not run out yet.
    let until = member
        .and_then(|m| m["communication_disabled_until"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp()))
        .or_else(|| guild.and_then(|g| world.guilds.get(g)).and_then(|g| g.members.get(user_id)).and_then(|m| m.timeout_until));
    let timed_out = until.is_some_and(|t| t > chrono::Utc::now().timestamp());
    let word = status_word(status);
    let relationship = if world.friends.contains(user_id) {
        "Friend"
    } else if world.blocked.contains(user_id) {
        "Blocked"
    } else if world.incoming.contains(user_id) {
        "Incoming"
    } else if world.outgoing.contains(user_id) {
        "Outgoing"
    } else {
        "None"
    };
    json!({
        "nick": nick,
        "userId": user_id,
        "prefix": "",
        "away": word == "offline",
        "status": word,
        "activity": custom.filter(|t| !t.is_empty()).map(|t| json!({ "kind": "custom", "text": t })).unwrap_or(Value::Null),
        "bot": user.bot,
        "relationship": relationship,
        "avatarUrl": avatar,
        "roles": roles,
        "hoist": hoisted.map(|r| r.name.clone()),
        "hoistRank": hoisted.map(|r| r.position),
        "colour": colour,
        "timedOut": timed_out,
    })
}

/// Asks for a channel's list (and makes its guild active), or lists a private conversation's people at once.
pub async fn refresh(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<usize> {
    let channel_id = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Fluxer conversation"))?;
    let channel = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(channel_id).cloned().ok_or_else(|| anyhow!("Fluxer has no such channel"))?;
    match (&channel.kind, &channel.guild) {
        (_, Some(guild)) => {
            super::connect::activate_for(handle, channel_id);
            let subscription = json!({ "active": true, "typing": true, "member_list_channels": { channel_id: [WINDOW] } });
            let _ = handle.gateway.send(json!({ "op": 14, "d": { "subscriptions": { guild: subscription } } }).to_string());
            Ok(0)
        }
        (Kind::Direct | Kind::Group | Kind::Notes, None) => {
            let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            let mut people: Vec<String> = channel.recipients.clone();
            if !people.contains(&world.own_id) {
                people.push(world.own_id.clone());
            }
            let list: Vec<Value> = people
                .iter()
                .map(|id| {
                    let (status, custom) = world.presence.get(id).cloned().unwrap_or_else(|| (if *id == world.own_id { "online".to_string() } else { "offline".to_string() }, None));
                    entry(&world, handle, None, id, None, &status, custom.as_deref())
                })
                .collect();
            drop(world);
            Ok(publish(state, buffer, list.clone(), list.len()))
        }
        _ => Ok(0),
    }
}

/// A channel's list as Fluxer sent it, for every conversation showing that channel.
pub fn list_update(state: &AppState, handle: &Handle, data: &Value) {
    let Some(channel) = data["channel_id"].as_str().or_else(|| data["id"].as_str()) else { return };
    let guild = data["guild_id"].as_str();
    let total = data["member_count"].as_u64().unwrap_or(0) as usize;
    let list: Vec<Value> = {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for op in data["ops"].as_array().into_iter().flatten() {
            for item in op["items"].as_array().into_iter().flatten() {
                let m = &item["member"];
                let Some(id) = m["user"]["id"].as_str() else { continue };
                if let Some(g) = guild {
                    world.put_member(g, m);
                }
                let status = m["presence"]["status"].as_str().unwrap_or("offline").to_string();
                let custom = m["presence"]["custom_status"]["text"].as_str().map(str::to_string);
                world.presence.insert(id.to_string(), (status.clone(), custom.clone()));
                out.push(entry(&world, handle, guild, id, Some(m), &status, custom.as_deref()));
            }
        }
        out
    };
    if let Some(buffer) = super::messages::buffer_of(state, &handle.account_id, channel) {
        publish(state, &buffer, list, total);
    }
}

/// Somebody's presence changed: kept, and wherever they are listed, so they are now.
pub fn presence(state: &AppState, handle: &Handle, data: &Value) {
    let Some(user_id) = data["user"]["id"].as_str() else { return };
    let status = data["status"].as_str().unwrap_or("offline").to_string();
    let custom = data["custom_status"]["text"].as_str().map(str::to_string);
    handle.world.lock().unwrap_or_else(|e| e.into_inner()).presence.insert(user_id.to_string(), (status.clone(), custom.clone()));
    for buffer in state.runtime.list_buffers() {
        if buffer.account_id != handle.account_id {
            continue;
        }
        let Some(existing) = state.runtime.get_presence(&buffer.id) else { continue };
        let Some(members) = existing.as_array() else { continue };
        if !members.iter().any(|m| m["userId"].as_str() == Some(user_id)) {
            continue;
        }
        let word = status_word(&status);
        let updated: Vec<Value> = members
            .iter()
            .map(|m| {
                if m["userId"].as_str() != Some(user_id) {
                    return m.clone();
                }
                let mut m = m.clone();
                m["status"] = json!(word);
                m["away"] = json!(word == "offline");
                m["activity"] = custom.as_ref().filter(|t| !t.is_empty()).map(|t| json!({ "kind": "custom", "text": t })).unwrap_or(Value::Null);
                m
            })
            .collect();
        let list = json!(updated);
        state.runtime.set_presence(&buffer.id, list.clone());
        state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": list }));
    }
}

/// A guild's roles or somebody's roles in it changed: every list shown for one of its channels is drawn again from
/// what is known, so a role given, coloured or listed apart shows at once. `only` is the one person who changed, or
/// none for everybody (a role itself changed).
pub fn redraw_guild(state: &AppState, handle: &Handle, guild: &str, only: Option<&str>) {
    for buffer in state.runtime.list_buffers() {
        if buffer.account_id != handle.account_id || !buffer.group_id.as_deref().is_some_and(|g| g.ends_with(&format!("|guild:{guild}"))) {
            continue;
        }
        let Some(existing) = state.runtime.get_presence(&buffer.id) else { continue };
        let Some(members) = existing.as_array() else { continue };
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let Some(g) = world.guilds.get(guild) else { continue };
        let mut changed = false;
        let updated: Vec<Value> = members
            .iter()
            .map(|m| {
                let Some(id) = m["userId"].as_str().filter(|id| only.is_none_or(|o| o == *id)) else { return m.clone() };
                let Some(member) = g.members.get(id) else { return m.clone() };
                changed = true;
                let shape = json!({ "nick": member.nick, "roles": member.roles });
                let (status, custom) = world.presence.get(id).cloned().unwrap_or_else(|| (m["status"].as_str().unwrap_or("offline").to_string(), None));
                entry(&world, handle, Some(guild), id, Some(&shape), &status, custom.as_deref())
            })
            .collect();
        drop(world);
        if changed {
            let list = json!(updated);
            state.runtime.set_presence(&buffer.id, list.clone());
            state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": list }));
        }
    }
}

fn publish(state: &AppState, buffer: &Buffer, list: Vec<Value>, total: usize) -> usize {
    let list = Value::Array(list);
    state.runtime.set_presence(&buffer.id, list.clone());
    state.runtime.set_presence_total(&buffer.id, total);
    state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": list, "total": total }));
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_presence_word_is_the_windows() {
        assert_eq!(status_word("online"), "online");
        assert_eq!(status_word("dnd"), "dnd");
        assert_eq!(status_word("invisible"), "offline");
        assert_eq!(status_word(""), "offline");
    }
}
