//! The account's own people: opening a conversation with somebody, friends and their requests, blocking, and joining
//! and leaving guilds.
//!
//! A relationship is Fluxer's record of another account (`http-api/users/relationships.mdx`): friend, blocked, a request
//! asked of this account, or one it asked. What the gateway says as they change is what the invitations tile lists -
//! everybody whose request is waiting on this account.

use super::events;
use super::media;
use super::messages;
use super::world::Kind;
use super::Handle;
use crate::state::AppState;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashSet;

/// The code in what somebody pasted: a bare code, or a link such as `https://fluxer.gg/Code` or
/// `https://fluxer.com/invite/Code` (the instance's own invite address, whichever it is).
pub fn invite_code(typed: &str) -> Option<String> {
    let text = typed.trim().trim_end_matches('/');
    let code = match url::Url::parse(text) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => {
            let parts: Vec<&str> = url.path_segments()?.filter(|p| !p.is_empty()).collect();
            match parts.as_slice() {
                [.., "invite", code] => (*code).to_string(),
                [code] => (*code).to_string(),
                _ => return None,
            }
        }
        _ => text.to_string(),
    };
    (!code.is_empty() && code.len() <= 64 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then_some(code)
}

/// Opens the conversation with somebody (or the account's own notes, for itself), and answers its buffer id.
pub async fn open_dm(state: &AppState, handle: &Handle, user: &str) -> Result<String> {
    let own = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone();
    let channel = if user == own {
        // The account's notes are a channel whose id is its own (type 999), there from the start.
        handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(&own).cloned().ok_or_else(|| anyhow!("this instance has no personal notes"))?
    } else {
        let made = handle.api.post("/users/@me/channels", &json!({ "recipient_id": user })).await.map_err(|e| anyhow!("{e}"))?;
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let id = world.put_channel(&made).ok_or_else(|| anyhow!("Fluxer did not say where that conversation is"))?;
        world.channels.get(&id).cloned().ok_or_else(|| anyhow!("Fluxer did not say where that conversation is"))?
    };
    events::register_channel(state, handle, &channel, 0, None);
    messages::buffer_of(state, &handle.account_id, &channel.id).map(|b| b.id).ok_or_else(|| anyhow!("the conversation did not come up"))
}

/// Joins a guild (or a group) by its invite. Fluxer says the guild on the gateway as it is joined (`GUILD_CREATE`),
/// which makes its conversations; a group's channel comes back here.
pub async fn join_invite(state: &AppState, handle: &Handle, typed: &str) -> Result<()> {
    let code = invite_code(typed).ok_or_else(|| anyhow!("\"{}\" is not a Fluxer invite link or code", typed.trim()))?;
    let held = handle.world.lock().unwrap_or_else(|e| e.into_inner()).guilds.len();
    if held >= handle.limits.guilds {
        bail!("This account is in {held} communities, the most Fluxer allows it here");
    }
    let joined = handle.api.post(&format!("/invites/{code}"), &json!({})).await.map_err(|e| anyhow!("{e}"))?;
    if joined["guild"].is_null() && joined["channel"].is_object() {
        let channel = {
            let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            world.put_channel(&joined["channel"]).and_then(|id| world.channels.get(&id).cloned())
        };
        if let Some(channel) = channel {
            events::register_channel(state, handle, &channel, 0, None);
        }
    }
    Ok(())
}

/// Leaves a guild. One the account owns is not left: Fluxer would have it handed over or deleted first.
pub async fn leave_guild(state: &AppState, handle: &Handle, guild: &str) -> Result<()> {
    {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if world.guilds.get(guild).is_some_and(|g| g.owner == world.own_id) {
            bail!("You own this community. Hand it to somebody else or delete it from Fluxer's own app if you mean to leave it");
        }
    }
    handle.api.delete(&format!("/users/@me/guilds/{guild}")).await.map_err(|e| anyhow!("{e}"))?;
    events::handle(state, handle, "GUILD_DELETE", &json!({ "id": guild }));
    Ok(())
}

fn listed(handle: &Handle, ids: &HashSet<String>) -> Vec<Value> {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let mut list: Vec<(String, Value)> = ids
        .iter()
        .map(|id| {
            let user = world.users.get(id).cloned().unwrap_or_default();
            let avatar = user.avatar.as_deref().and_then(|h| media::asset_url(&handle.instance, "avatars", id, h)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, Some("image/webp")));
            let (status, custom) = world.presence.get(id).cloned().unwrap_or(("offline".to_string(), None));
            let name = user.shown();
            (name.to_lowercase(), json!({ "id": id, "name": if name.is_empty() { id.clone() } else { name }, "avatarUrl": avatar, "online": status != "offline", "statusText": custom }))
        })
        .collect();
    list.sort_by(|a, b| a.0.cmp(&b.0));
    list.into_iter().map(|(_, v)| v).collect()
}

/// Everybody on this account's friends page: requests asked of it, its friends, those it asked, and who it blocked.
pub fn page(handle: &Handle) -> Value {
    let (incoming, friends, outgoing, blocked) = {
        let w = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        (w.incoming.clone(), w.friends.clone(), w.outgoing.clone(), w.blocked.clone())
    };
    json!({ "incoming": listed(handle, &incoming), "friends": listed(handle, &friends), "outgoing": listed(handle, &outgoing), "blocked": listed(handle, &blocked) })
}

/// Says the lists of people, so the invitations tile and the friends page are right. The event's name is the one the
/// window already listens to for Stoat's, whose lists have the same shape.
pub fn tell_requests(state: &AppState, handle: &Handle) {
    let mut page = page(handle);
    page["accountId"] = json!(handle.account_id);
    state.events.emit("stoatFriendRequests", page);
}

/// A direct message with somebody whose relationship changed: it may now be one that cannot be written in, or can again.
pub fn redraw_direct(state: &AppState, handle: &Handle, user: &str) {
    let direct: Vec<_> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.values().filter(|c| c.kind == Kind::Direct && c.recipients.iter().any(|r| r == user)).cloned().collect();
    for channel in direct {
        events::register_channel(state, handle, &channel, 0, None);
    }
}

/// Asks somebody to be a friend by their name (`name`, or `name#0001` where the instance tags names).
pub async fn add_friend(handle: &Handle, username: &str) -> Result<()> {
    let name = username.trim().trim_start_matches('@');
    if name.is_empty() {
        bail!("Type their username");
    }
    let body = match name.split_once('#') {
        Some((user, tag)) => json!({ "username": user, "discriminator": tag }),
        None => json!({ "username": name }),
    };
    handle.api.post("/users/@me/relationships", &body).await.map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

/// Accepts somebody's request, or turns it down, takes a request back, or ends a friendship.
pub async fn answer_friend(state: &AppState, handle: &Handle, user: &str, accept: bool) -> Result<()> {
    let path = format!("/users/@me/relationships/{user}");
    if accept {
        let r = handle.api.put(&path, Some(&json!({}))).await.map_err(|e| anyhow!("{e}"))?;
        handle.world.lock().unwrap_or_else(|e| e.into_inner()).set_relationship(user, r["type"].as_i64().or(Some(1)));
    } else {
        handle.api.delete(&path).await.map_err(|e| anyhow!("{e}"))?;
        handle.world.lock().unwrap_or_else(|e| e.into_inner()).set_relationship(user, None);
    }
    tell_requests(state, handle);
    Ok(())
}

/// Blocks somebody, or lets them through again.
pub async fn block(state: &AppState, handle: &Handle, user: &str, blocked: bool) -> Result<()> {
    let path = format!("/users/@me/relationships/{user}");
    if blocked {
        handle.api.put(&path, Some(&json!({ "type": 2 }))).await.map_err(|e| anyhow!("{e}"))?;
        handle.world.lock().unwrap_or_else(|e| e.into_inner()).set_relationship(user, Some(2));
    } else {
        handle.api.delete(&path).await.map_err(|e| anyhow!("{e}"))?;
        handle.world.lock().unwrap_or_else(|e| e.into_inner()).set_relationship(user, None);
    }
    redraw_direct(state, handle, user);
    tell_requests(state, handle);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invite_is_read_from_a_link_or_a_bare_code() {
        assert_eq!(invite_code("QGEnmqo9").as_deref(), Some("QGEnmqo9"));
        assert_eq!(invite_code("https://fluxer.gg/QGEnmqo9").as_deref(), Some("QGEnmqo9"));
        assert_eq!(invite_code("https://fluxer.com/invite/QGEnmqo9/").as_deref(), Some("QGEnmqo9"));
        assert_eq!(invite_code("http://localhost:8890/invite/abc-123_x").as_deref(), Some("abc-123_x"));
        assert_eq!(invite_code("https://example.com/a/b"), None);
        assert_eq!(invite_code(""), None);
        assert_eq!(invite_code("not a code!"), None);
    }
}
