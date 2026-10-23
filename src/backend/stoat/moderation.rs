//! What this account may do to a server's members, and doing it: removing, barring, silencing, and giving or taking
//! roles.
//!
//! What the member menu offers is read off this account's own permissions in the conversation (they are advisory: Stoat
//! re-checks and says why in words, as it does for any client). Roles are held in order - a lower rank number is more
//! powerful - and an account may only hand out roles below its own best, which is Stoat's rule.

use super::world::{ASSIGN_ROLES, BAN_MEMBERS, KICK_MEMBERS, MANAGE_MESSAGES, TIMEOUT_MEMBERS};
use super::Handle;
use crate::model::Buffer;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

/// The server a conversation belongs to.
fn server_of(handle: &Handle, buffer: &Buffer) -> Result<String> {
    let channel = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Stoat conversation"))?;
    handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(channel).and_then(|c| c.server.clone()).ok_or_else(|| anyhow!("this conversation is not in a server"))
}

/// What the menu may offer: the moderating this account's permissions allow, and the roles it may hand out.
pub fn powers(handle: &Handle, buffer: &Buffer) -> Value {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let Some(channel) = buffer.remote_id.as_deref().and_then(|c| world.channels.get(c)) else { return json!({}) };
    let Some(server) = channel.server.as_ref().and_then(|s| world.servers.get(s)) else { return json!({}) };
    let permissions = world.permissions(channel, chrono::Utc::now().timestamp());
    let mine = world.own_rank(server);
    let mut roles: Vec<(&String, &super::world::Role)> = server.roles.iter().filter(|(_, r)| mine.is_some_and(|m| r.rank > m)).collect();
    roles.sort_by_key(|(_, r)| r.rank);
    json!({
        "canKick": permissions & KICK_MEMBERS != 0,
        "canBan": permissions & BAN_MEMBERS != 0,
        "canMute": permissions & TIMEOUT_MEMBERS != 0,
        "canAssignRoles": permissions & ASSIGN_ROLES != 0,
        "canRedactOthers": permissions & MANAGE_MESSAGES != 0,
        "roles": if permissions & ASSIGN_ROLES != 0 { roles.into_iter().map(|(id, r)| json!({ "id": id, "name": r.name, "colour": r.colour })).collect::<Vec<_>>() } else { Vec::new() },
    })
}

/// When a timeout of this many minutes ends, as Stoat reads it.
pub fn timeout_until(now: i64, minutes: i64) -> String {
    chrono::DateTime::from_timestamp(now + minutes.max(1) * 60, 0).map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()).unwrap_or_default()
}

/// Removes, bars or silences a member (`kick`, `ban`, `mute`), or lets a silenced one speak again (`unmute`).
pub async fn moderate(handle: &Handle, buffer: &Buffer, user: &str, action: &str, minutes: Option<i64>, reason: Option<&str>) -> Result<()> {
    let server = server_of(handle, buffer)?;
    let member = format!("/servers/{server}/members/{user}");
    match action {
        "kick" => handle.api.delete(&member).await.map(|_| ()),
        "ban" => handle.api.put(&format!("/servers/{server}/bans/{user}"), Some(&json!({ "reason": reason }))).await.map(|_| ()),
        "unban" => handle.api.delete(&format!("/servers/{server}/bans/{user}")).await.map(|_| ()),
        "mute" => handle.api.patch(&member, &json!({ "timeout": timeout_until(chrono::Utc::now().timestamp(), minutes.unwrap_or(10)) })).await.map(|_| ()),
        "unmute" => handle.api.patch(&member, &json!({ "remove": ["Timeout"] })).await.map(|_| ()),
        other => bail!("Stoat has no way to {other} a member"),
    }
    .map_err(|e| anyhow!("{e}"))
}

/// The roles somebody holds after one is given or taken: the others they held stay, and a role is never held twice.
pub fn roles_after(held: &[String], role: &str, give: bool) -> Vec<String> {
    let mut out: Vec<String> = held.iter().filter(|r| r.as_str() != role).cloned().collect();
    if give {
        out.push(role.to_string());
    }
    out
}

/// Gives a member a role, or takes it away.
pub async fn set_role(handle: &Handle, buffer: &Buffer, user: &str, role: &str, give: bool) -> Result<()> {
    let server = server_of(handle, buffer)?;
    let member = handle.api.get(&format!("/servers/{server}/members/{user}"), &[]).await.map_err(|e| anyhow!("{e}"))?;
    let held: Vec<String> = member["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(str::to_string)).collect();
    let roles = roles_after(&held, role, give);
    handle.api.patch(&format!("/servers/{server}/members/{user}"), &json!({ "roles": roles })).await.map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

/// Makes an invite to a channel's server, and answers the link to give out.
pub async fn invite(handle: &Handle, buffer: &Buffer, web: &str) -> Result<String> {
    let channel = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Stoat conversation"))?;
    let made = handle.api.post(&format!("/channels/{channel}/invites"), &json!({})).await.map_err(|e| anyhow!("{e}"))?;
    let code = made["_id"].as_str().ok_or_else(|| anyhow!("Stoat did not give an invite"))?;
    Ok(format!("{}/invite/{code}", web.trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_ends_the_stated_minutes_from_now_in_the_form_stoat_reads() {
        assert_eq!(timeout_until(0, 10), "1970-01-01T00:10:00.000Z");
        assert_eq!(timeout_until(1_000_000_000, 60), "2001-09-09T02:46:40.000Z");
        assert_eq!(timeout_until(0, 0), "1970-01-01T00:01:00.000Z", "never less than a minute");
    }

    #[test]
    fn a_role_is_added_once_and_taken_away_without_touching_the_rest() {
        let held = vec!["A".to_string(), "B".to_string()];
        assert_eq!(roles_after(&held, "C", true), vec!["A", "B", "C"]);
        assert_eq!(roles_after(&held, "A", true), vec!["B", "A"], "not held twice");
        assert_eq!(roles_after(&held, "A", false), vec!["B"]);
        assert_eq!(roles_after(&held, "Z", false), vec!["A", "B"]);
    }
}
