//! Who is here: the list beside a conversation, and who is online.
//!
//! A server's members come in one answer (with the people they are), a group's are in its channel, and a direct
//! message is the two of them. Stoat tells a client who is online only for the servers it has asked about - the
//! account says it is looking at a server with `Subscribe` - and then sends each change as it happens, which is kept
//! here against whoever is listed.
//!
//! Each entry is what the window's member list reads: the name shown (the server nickname, else their display name),
//! their id, whether they are away, their status word, their picture and the roles they hold, highest rank first.

use super::media;
use super::world::{Kind, Server, User, World};
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;

/// A status as the window's words have it, from Stoat's presence: not online at all is offline, whatever the
/// presence word says.
pub fn status_of(user: &User) -> &'static str {
    if !user.online {
        return "offline";
    }
    match user.presence.as_str() {
        "Idle" => "idle",
        "Busy" => "dnd",
        "Focus" => "focus",
        "Invisible" => "offline",
        _ => "online",
    }
}

/// One person as a member-list entry.
pub fn entry(world: &World, handle: &Handle, user_id: &str, member: Option<&Value>, server: Option<&Server>) -> Value {
    let user = world.users.get(user_id).cloned().unwrap_or_default();
    let status = status_of(&user);
    let nick = member.and_then(|m| m["nickname"].as_str()).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| world.name_of(user_id));
    let avatar = member
        .and_then(|m| m["avatar"]["_id"].as_str())
        .map(str::to_string)
        .or(user.avatar.clone())
        .and_then(|f| media::file_url(&handle.instance, "avatars", &f))
        .map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, None));
    let roles: Vec<String> = match (member, server) {
        (Some(m), Some(s)) => {
            let mut held: Vec<(&String, i64)> = m["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str()).filter_map(|r| s.roles.get(r).map(|role| (&role.name, role.rank))).collect();
            held.sort_by_key(|(_, rank)| *rank);
            held.into_iter().map(|(name, _)| name.clone()).collect()
        }
        _ => Vec::new(),
    };
    json!({
        "nick": nick,
        "userId": user_id,
        "prefix": "",
        "away": status == "offline",
        "status": status,
        "activity": user.status_text.as_ref().map(|t| json!({ "kind": "custom", "text": t })).unwrap_or(Value::Null),
        "bot": user.bot,
        "relationship": user.relationship,
        "avatarUrl": avatar,
        "roles": roles,
    })
}

/// Tells Stoat the account is looking at a server, so it sends who comes online and goes offline there.
pub fn subscribe(handle: &Handle, server: &str) {
    let _ = handle.socket.send(json!({ "type": "Subscribe", "server_id": server }).to_string());
}

/// The most people drawn in one list. A server can have a hundred thousand; nobody scrolls that, and drawing it (every name,
/// every picture) is what stopped the window.
pub const LISTED: usize = 1000;
/// A server with this many people online or more is too big to ask for its offline ones as well.
const SMALL: usize = 300;
/// The most that is read of a server's whole list of people before it is given up on.
const WHOLE_BYTES: usize = 6_000_000;
/// How long the list of a large server is kept before it is made again.
const KEEP_LARGE: std::time::Duration = std::time::Duration::from_secs(60);

/// The people of a server: those online, and - where that is few enough for the whole to be small - everybody. The
/// answer says whether it is everybody.
async fn server_people(handle: &Handle, server_id: &str) -> Result<(Value, bool)> {
    let online = handle.api.get(&format!("/servers/{server_id}/members"), &[("exclude_offline", "true".to_string())]).await.map_err(|e| anyhow!("{e}"))?;
    if online["members"].as_array().map_or(0, Vec::len) >= SMALL {
        return Ok((online, false));
    }
    match handle.api.get_capped(&format!("/servers/{server_id}/members"), &[("exclude_offline", "false".to_string())], WHOLE_BYTES).await {
        Ok(everybody) => Ok((everybody, true)),
        // Too many to read, or not to be had: those online are who is shown.
        Err(_) => Ok((online, false)),
    }
}

/// The members to draw out of an answer, the most senior first and no more than `LISTED`: by the rank of the best role each
/// holds, then by name.
pub fn most_senior(world: &World, server: Option<&Server>, members: &[Value]) -> Vec<Value> {
    let rank = |m: &Value| -> i64 {
        server
            .map(|s| m["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str()).filter_map(|r| s.roles.get(r)).map(|r| r.rank).min().unwrap_or(i64::MAX))
            .unwrap_or(i64::MAX)
    };
    let name = |m: &Value| m["_id"]["user"].as_str().map(|id| world.name_of(id).to_lowercase()).unwrap_or_default();
    let mut keyed: Vec<(i64, String, &Value)> = members.iter().map(|m| (rank(m), name(m), m)).collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    keyed.into_iter().take(LISTED).map(|(_, _, m)| m.clone()).collect()
}

/// Lists the people of a conversation and tells the window.
pub async fn refresh(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<usize> {
    let channel_id = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Stoat conversation"))?;
    let channel = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(channel_id).cloned().ok_or_else(|| anyhow!("Stoat has no such channel"))?;
    let list: Vec<Value> = match (&channel.kind, &channel.server) {
        (Kind::Text | Kind::Voice, Some(server_id)) => {
            subscribe(handle, server_id);
            // A large server's list was made a moment ago: it is the same list, and making it again is thousands of people read.
            let kept = handle.world.lock().unwrap_or_else(|e| e.into_inner()).large.get(server_id).filter(|(at, _, _)| at.elapsed() < KEEP_LARGE).map(|(_, list, total)| (list.clone(), *total));
            if let Some((list, total)) = kept {
                return Ok(publish(state, buffer, list, total));
            }
            let (answer, everybody) = server_people(handle, server_id).await?;
            let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            let members: Vec<Value> = answer["members"].as_array().cloned().unwrap_or_default();
            let found = members.len();
            let users: HashMap<&str, &Value> = answer["users"].as_array().into_iter().flatten().filter_map(|u| u["_id"].as_str().map(|id| (id, u))).collect();
            // Only the people to be drawn are taken into the world: the rest are names nobody will see.
            for m in &members {
                if let Some((id, user)) = m["_id"]["user"].as_str().and_then(|id| users.get(id)).and_then(|u| super::world::parse_user(u)) {
                    world.users.insert(id, user);
                }
            }
            let server = world.servers.get(server_id).cloned();
            let shown = most_senior(&world, server.as_ref(), &members);
            let list: Vec<Value> = shown.iter().filter_map(|m| m["_id"]["user"].as_str().map(|id| entry(&world, handle, id, Some(m), server.as_ref()))).collect();
            if everybody && found <= LISTED {
                world.large.remove(server_id);
            } else {
                world.large.insert(server_id.to_string(), (std::time::Instant::now(), list.clone(), found));
            }
            drop(world);
            return Ok(publish(state, buffer, list, found));
        }
        (Kind::Direct | Kind::Group, _) => {
            let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            channel.recipients.iter().map(|id| entry(&world, handle, id, None, None)).collect()
        }
        _ => Vec::new(),
    };
    let total = list.len();
    Ok(publish(state, buffer, list, total))
}

/// Gives the window a conversation's list of people, and how many people it is of (which is more than are listed where
/// the server is large).
fn publish(state: &AppState, buffer: &Buffer, list: Vec<Value>, total: usize) -> usize {
    let list = Value::Array(list);
    state.runtime.set_presence(&buffer.id, list.clone());
    state.runtime.set_presence_total(&buffer.id, total);
    state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": list, "total": total }));
    total
}

/// Somebody's status changed: wherever they are listed, so they are now.
pub fn update_status(state: &AppState, handle: &Handle, user_id: &str) {
    let user = handle.world.lock().unwrap_or_else(|e| e.into_inner()).users.get(user_id).cloned();
    let Some(user) = user else { return };
    let status = status_of(&user);
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
                m["status"] = json!(status);
                m["away"] = json!(status == "offline");
                m["activity"] = user.status_text.as_ref().map(|t| json!({ "kind": "custom", "text": t })).unwrap_or(Value::Null);
                m
            })
            .collect();
        let list = json!(updated);
        state.runtime.set_presence(&buffer.id, list.clone());
        state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": list }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(online: bool, presence: &str) -> User {
        User { online, presence: presence.to_string(), ..Default::default() }
    }

    #[test]
    fn a_huge_server_is_listed_by_rank_then_name_and_no_further_than_the_limit() {
        let mut w = World::default();
        w.absorb_ready("ME", &json!({ "servers": [{ "_id": "S", "owner": "O", "name": "S", "roles": { "R1": { "name": "Mods", "rank": 1 }, "R9": { "name": "Fans", "rank": 9 } } }] }));
        let mut members: Vec<Value> = (0..(LISTED + 50)).map(|i| json!({ "_id": { "server": "S", "user": format!("U{i:05}") }, "roles": [] })).collect();
        for m in &members {
            let id = m["_id"]["user"].as_str().unwrap().to_string();
            w.users.insert(id.clone(), User { name: format!("name{id}"), ..Default::default() });
        }
        members.push(json!({ "_id": { "server": "S", "user": "BOSS" }, "roles": ["R1"] }));
        members.push(json!({ "_id": { "server": "S", "user": "FAN" }, "roles": ["R9"] }));
        w.users.insert("BOSS".into(), User { name: "zed".into(), ..Default::default() });
        w.users.insert("FAN".into(), User { name: "amy".into(), ..Default::default() });
        let server = w.servers.get("S").cloned();
        let shown = most_senior(&w, server.as_ref(), &members);
        assert_eq!(shown.len(), LISTED);
        assert_eq!(shown[0]["_id"]["user"], "BOSS", "a moderator before anybody, whatever their name");
        assert_eq!(shown[1]["_id"]["user"], "FAN", "then a role, then those with none by name");
        assert_eq!(shown[2]["_id"]["user"], "U00000");
    }

    #[test]
    fn a_status_is_offline_unless_the_person_is_online_and_then_the_word_they_chose() {
        assert_eq!(status_of(&user(false, "Online")), "offline");
        assert_eq!(status_of(&user(true, "Online")), "online");
        assert_eq!(status_of(&user(true, "Idle")), "idle");
        assert_eq!(status_of(&user(true, "Busy")), "dnd");
        assert_eq!(status_of(&user(true, "Focus")), "focus");
        assert_eq!(status_of(&user(true, "Invisible")), "offline", "somebody invisible is not shown as here");
        assert_eq!(status_of(&user(true, "")), "online");
    }
}
