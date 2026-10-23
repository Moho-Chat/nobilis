//! The account's own people: opening a conversation with somebody, friends and their requests, blocking, and joining
//! and leaving servers.
//!
//! A friendship is made by one side asking and the other agreeing (`PUT /users/{id}/friend` does either), taken back
//! or refused by `DELETE`. What Stoat sends as the relationship changes (`UserRelationship`) is what the invitations
//! tile lists: everybody whose relationship to this account is `Incoming`.

use super::events;
use super::media;
use super::messages;
use super::world::{parse_channel, parse_server, Kind};
use super::Handle;
use crate::state::AppState;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

/// The code in what somebody pasted: a bare code, or a link such as `https://stoat.chat/invite/Code`.
pub fn invite_code(typed: &str) -> Option<String> {
    let text = typed.trim().trim_end_matches('/');
    let code = match url::Url::parse(text) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => {
            let parts: Vec<&str> = url.path_segments()?.filter(|p| !p.is_empty()).collect();
            match parts.as_slice() {
                [.., "invite", code] => (*code).to_string(),
                [code] if url.host_str().is_some_and(|h| h.ends_with("rvlt.gg") || h.ends_with("stoat.gg")) => (*code).to_string(),
                _ => return None,
            }
        }
        _ => text.to_string(),
    };
    (!code.is_empty() && code.len() <= 32 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then_some(code)
}

/// Opens the conversation with somebody (or the account's own notes, for itself), and answers its buffer id.
pub async fn open_dm(state: &AppState, handle: &Handle, user: &str) -> Result<String> {
    let channel = handle.api.get(&format!("/users/{user}/dm"), &[]).await.map_err(|e| anyhow!("{e}"))?;
    let parsed = parse_channel(&channel).ok_or_else(|| anyhow!("Stoat did not say where that conversation is"))?;
    // Who they are, for the name it is called by.
    let known = handle.world.lock().unwrap_or_else(|e| e.into_inner()).users.contains_key(user);
    if !known {
        if let Ok(found) = handle.api.get(&format!("/users/{user}"), &[]).await {
            if let Some((id, u)) = super::world::parse_user(&found) {
                handle.world.lock().unwrap_or_else(|e| e.into_inner()).users.insert(id, u);
            }
        }
    }
    handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.insert(parsed.id.clone(), parsed.clone());
    events::register_channel(state, handle, &parsed, 0, None);
    messages::buffer_of(state, &handle.account_id, &parsed.id).map(|b| b.id).ok_or_else(|| anyhow!("the conversation did not come up"))
}

/// Joins a server by its invite and makes its conversations.
pub async fn join_invite(state: &AppState, handle: &Handle, typed: &str) -> Result<()> {
    let code = invite_code(typed).ok_or_else(|| anyhow!("\"{}\" is not a Stoat invite link or code", typed.trim()))?;
    let joined = handle.api.post(&format!("/invites/{code}"), &json!({})).await.map_err(|e| anyhow!("{e}"))?;
    match joined["type"].as_str() {
        Some("Server") => {
            let server = parse_server(&joined["server"]).ok_or_else(|| anyhow!("Stoat did not describe that server"))?;
            {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                for c in joined["channels"].as_array().into_iter().flatten() {
                    if let Some(channel) = parse_channel(c) {
                        world.channels.insert(channel.id.clone(), channel);
                    }
                }
                world.members.entry(server.id.clone()).or_insert_with(|| super::world::Member { roles: Vec::new(), timeout: None, nickname: None, can_publish: true, can_receive: true });
                world.servers.insert(server.id.clone(), server.clone());
            }
            events::register_server_entry(state, handle, &server);
            Ok(())
        }
        Some("Group") => {
            if let Some(channel) = parse_channel(&joined["channel"]) {
                handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.insert(channel.id.clone(), channel.clone());
                events::register_channel(state, handle, &channel, 0, None);
            }
            Ok(())
        }
        _ => bail!("Stoat answered something moho does not know how to follow"),
    }
}

/// Leaves a server the account does not own.
pub async fn leave_server(state: &AppState, handle: &Handle, server: &str) -> Result<()> {
    {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if world.servers.get(server).is_some_and(|s| s.owner == world.own_id) {
            bail!("You own this server, and leaving it would delete it for everybody. Delete it from Stoat's own app if you mean to");
        }
    }
    handle.api.delete(&format!("/servers/{server}")).await.map_err(|e| anyhow!("{e}"))?;
    // The events say it too; this is so the window need not wait for them.
    events::handle(state, handle, &json!({ "type": "ServerDelete", "id": server }));
    Ok(())
}

/// Everybody who has asked to be this account's friend, as the invitations tile lists them.
pub fn incoming(handle: &Handle) -> Vec<Value> {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let mut list: Vec<(&String, &super::world::User)> = world.users.iter().filter(|(_, u)| u.relationship == "Incoming").collect();
    list.sort_by_key(|(_, u)| u.name.to_lowercase());
    list.into_iter()
        .map(|(id, u)| {
            let avatar = u.avatar.as_deref().and_then(|f| media::file_url(&handle.instance, "avatars", f)).map(|url| media::cached(&handle.account_id, media::AVATARS, &url, None, None));
            json!({ "id": id, "name": u.name, "avatarUrl": avatar })
        })
        .collect()
}

/// Says the list of requests, so the tile is right.
pub fn tell_requests(state: &AppState, handle: &Handle) {
    state.events.emit("stoatFriendRequests", json!({ "accountId": handle.account_id, "incoming": incoming(handle) }));
}

/// Asks somebody to be a friend by their `name#1234`.
pub async fn add_friend(handle: &Handle, username: &str) -> Result<()> {
    let name = username.trim().trim_start_matches('@');
    if !name.contains('#') || name.len() < 3 {
        bail!("Type their name with the number after it, like name#1234");
    }
    handle.api.post("/users/friend", &json!({ "username": name })).await.map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

/// Accepts or turns down somebody's request, or takes a friend back.
pub async fn answer_friend(state: &AppState, handle: &Handle, user: &str, accept: bool) -> Result<()> {
    let path = format!("/users/{user}/friend");
    let answer = if accept { handle.api.put(&path, None).await } else { handle.api.delete(&path).await };
    let updated = answer.map_err(|e| anyhow!("{e}"))?;
    // What Stoat says the relationship is now (the person, as it sees them), kept as the event would have.
    if let Some((id, parsed)) = super::world::parse_user(&updated) {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world.users.entry(id).and_modify(|u| u.relationship = parsed.relationship.clone()).or_insert(parsed);
    } else if !accept {
        if let Some(u) = handle.world.lock().unwrap_or_else(|e| e.into_inner()).users.get_mut(user) {
            u.relationship = "None".to_string();
        }
    }
    tell_requests(state, handle);
    Ok(())
}

/// Blocks somebody, or lets them through again.
pub async fn block(state: &AppState, handle: &Handle, user: &str, blocked: bool) -> Result<()> {
    let path = format!("/users/{user}/block");
    let answer = if blocked { handle.api.put(&path, None).await } else { handle.api.delete(&path).await };
    let updated = answer.map_err(|e| anyhow!("{e}"))?;
    if let Some((id, parsed)) = super::world::parse_user(&updated) {
        handle.world.lock().unwrap_or_else(|e| e.into_inner()).users.entry(id).and_modify(|u| u.relationship = parsed.relationship.clone()).or_insert(parsed);
    }
    // A conversation with them may now be one it cannot write in, or can again.
    let direct: Vec<_> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.values().filter(|c| c.kind == Kind::Direct && c.recipients.iter().any(|r| r == user)).cloned().collect();
    for channel in direct {
        events::register_channel(state, handle, &channel, 0, None);
    }
    tell_requests(state, handle);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invite_is_read_from_a_link_or_a_bare_code() {
        assert_eq!(invite_code("gJhyqXTt").as_deref(), Some("gJhyqXTt"));
        assert_eq!(invite_code("https://stoat.chat/invite/gJhyqXTt").as_deref(), Some("gJhyqXTt"));
        assert_eq!(invite_code("https://app.stoat.chat/invite/gJhyqXTt/").as_deref(), Some("gJhyqXTt"));
        assert_eq!(invite_code("http://localhost:8880/invite/abc-123_x").as_deref(), Some("abc-123_x"));
        assert_eq!(invite_code("https://stoat.gg/Abc123").as_deref(), Some("Abc123"));
        assert_eq!(invite_code("https://example.com/somewhere"), None);
        assert_eq!(invite_code(""), None);
        assert_eq!(invite_code("not a code!"), None);
        assert_eq!(invite_code("../../etc/passwd"), None);
    }
}
