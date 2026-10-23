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

/// A status as the window's words have it, from Stoat's presence: not online at all is offline, whatever the
/// presence word says.
pub fn status_of(user: &User) -> &'static str {
    if !user.online {
        return "offline";
    }
    match user.presence.as_str() {
        "Idle" => "idle",
        "Busy" => "dnd",
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
        "avatarUrl": avatar,
        "roles": roles,
    })
}

/// Tells Stoat the account is looking at a server, so it sends who comes online and goes offline there.
pub fn subscribe(handle: &Handle, server: &str) {
    let _ = handle.socket.send(json!({ "type": "Subscribe", "server_id": server }).to_string());
}

/// Lists the people of a conversation and tells the window.
pub async fn refresh(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<usize> {
    let channel_id = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Stoat conversation"))?;
    let channel = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(channel_id).cloned().ok_or_else(|| anyhow!("Stoat has no such channel"))?;
    let list: Vec<Value> = match (&channel.kind, &channel.server) {
        (Kind::Text | Kind::Voice, Some(server_id)) => {
            subscribe(handle, server_id);
            let answer = handle.api.get(&format!("/servers/{server_id}/members"), &[("exclude_offline", "false".to_string())]).await.map_err(|e| anyhow!("{e}"))?;
            let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            for u in answer["users"].as_array().into_iter().flatten() {
                if let Some((id, user)) = super::world::parse_user(u) {
                    world.users.insert(id, user);
                }
            }
            let server = world.servers.get(server_id).cloned();
            answer["members"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["_id"]["user"].as_str().map(|id| entry(&world, handle, id, Some(m), server.as_ref())))
                .collect()
        }
        (Kind::Direct | Kind::Group, _) => {
            let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            channel.recipients.iter().map(|id| entry(&world, handle, id, None, None)).collect()
        }
        _ => Vec::new(),
    };
    let total = list.len();
    let list = Value::Array(list);
    state.runtime.set_presence(&buffer.id, list.clone());
    state.runtime.set_presence_total(&buffer.id, total);
    state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": list, "total": total }));
    Ok(total)
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
    fn a_status_is_offline_unless_the_person_is_online_and_then_the_word_they_chose() {
        assert_eq!(status_of(&user(false, "Online")), "offline");
        assert_eq!(status_of(&user(true, "Online")), "online");
        assert_eq!(status_of(&user(true, "Idle")), "idle");
        assert_eq!(status_of(&user(true, "Busy")), "dnd");
        assert_eq!(status_of(&user(true, "Focus")), "online");
        assert_eq!(status_of(&user(true, "Invisible")), "offline", "somebody invisible is not shown as here");
        assert_eq!(status_of(&user(true, "")), "online");
    }
}
